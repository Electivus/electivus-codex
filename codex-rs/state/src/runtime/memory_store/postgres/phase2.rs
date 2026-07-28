use super::super::PHASE2_SUCCESS_COOLDOWN_SECONDS;
use super::DEFAULT_RETRY_REMAINING;
use super::JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL;
use super::MEMORY_CONSOLIDATION_JOB_KEY;
use super::PostgresMemoryStore;
use crate::Phase2JobClaimOutcome;
use crate::Stage1Output;
use codex_protocol::ThreadId;
use sqlx::AssertSqlSafe;
use sqlx::Row;
use uuid::Uuid;

impl PostgresMemoryStore {
    pub(crate) async fn enqueue_global_consolidation(
        &self,
        input_watermark: i64,
    ) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        self.enqueue_global_consolidation_in_transaction(&mut transaction, input_watermark)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn try_claim_global_phase2_job(
        &self,
        worker_id: ThreadId,
        lease_seconds: i64,
    ) -> anyhow::Result<Phase2JobClaimOutcome> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (kind, job_key, thread_id, status, worker_id, ownership_token, \
             started_at, finished_at, lease_until, retry_at, retry_remaining, last_error, \
             input_watermark, last_success_watermark) \
             VALUES ($1, $2, NULL, 'pending', NULL, NULL, NULL, NULL, NULL, NULL, $3, NULL, 0, 0) \
             ON CONFLICT(kind, job_key) DO NOTHING",
            self.jobs_table
        )))
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .bind(DEFAULT_RETRY_REMAINING)
        .execute(&mut *transaction)
        .await?;

        let existing_job = sqlx::query(AssertSqlSafe(format!(
            "SELECT status, lease_until, retry_at, input_watermark, finished_at, last_error \
             FROM {} WHERE kind = $1 AND job_key = $2 FOR UPDATE",
            self.jobs_table
        )))
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .fetch_one(&mut *transaction)
        .await?;
        let now: i64 =
            sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
                .fetch_one(&mut *transaction)
                .await?;
        let lease_until = now.saturating_add(lease_seconds.max(0));
        let cooldown_cutoff = now.saturating_sub(PHASE2_SUCCESS_COOLDOWN_SECONDS);

        let status: String = existing_job.try_get("status")?;
        let existing_lease_until: Option<i64> = existing_job.try_get("lease_until")?;
        let retry_at: Option<i64> = existing_job.try_get("retry_at")?;
        let finished_at: Option<i64> = existing_job.try_get("finished_at")?;
        let last_error: Option<String> = existing_job.try_get("last_error")?;
        if retry_at.is_some_and(|retry_at| retry_at > now) {
            transaction.commit().await?;
            return Ok(Phase2JobClaimOutcome::SkippedRetryUnavailable);
        }
        if status == "running" && existing_lease_until.is_some_and(|lease_until| lease_until > now)
        {
            transaction.commit().await?;
            return Ok(Phase2JobClaimOutcome::SkippedRunning);
        }
        if last_error.is_none()
            && finished_at.is_some_and(|finished_at| finished_at > cooldown_cutoff)
        {
            transaction.commit().await?;
            return Ok(Phase2JobClaimOutcome::SkippedCooldown);
        }

        let ownership_token = Uuid::new_v4().to_string();
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET status = 'running', worker_id = $1, ownership_token = $2, \
             started_at = $3, finished_at = NULL, lease_until = $4, retry_at = NULL, \
             last_error = NULL WHERE kind = $5 AND job_key = $6",
            self.jobs_table
        )))
        .bind(worker_id.to_string())
        .bind(&ownership_token)
        .bind(now)
        .bind(lease_until)
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .execute(&mut *transaction)
        .await?;
        let input_watermark = existing_job
            .try_get::<Option<i64>, _>("input_watermark")?
            .unwrap_or(0);
        transaction.commit().await?;
        Ok(Phase2JobClaimOutcome::Claimed {
            ownership_token,
            input_watermark,
        })
    }

    pub(crate) async fn heartbeat_global_phase2_job(
        &self,
        ownership_token: &str,
        lease_seconds: i64,
    ) -> anyhow::Result<bool> {
        let rows_affected = sqlx::query(AssertSqlSafe(format!(
            "WITH db_clock AS ( \
             SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint AS now \
             ) UPDATE {} AS job SET lease_until = db_clock.now + $1 FROM db_clock \
             WHERE job.kind = $2 AND job.job_key = $3 AND job.status = 'running' \
             AND job.ownership_token = $4 AND job.lease_until > db_clock.now",
            self.jobs_table
        )))
        .bind(lease_seconds.max(0))
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .bind(ownership_token)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows_affected > 0)
    }

    pub(crate) async fn mark_global_phase2_job_succeeded(
        &self,
        ownership_token: &str,
        completed_watermark: i64,
        selected_outputs: &[Stage1Output],
    ) -> anyhow::Result<bool> {
        let mut transaction = self.pool.begin().await?;
        let completed = self
            .complete_global_phase2_job_in_transaction(
                &mut transaction,
                ownership_token,
                completed_watermark,
                selected_outputs,
            )
            .await?;
        transaction.commit().await?;
        Ok(completed)
    }

    pub(super) async fn complete_global_phase2_job_in_transaction(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        ownership_token: &str,
        completed_watermark: i64,
        selected_outputs: &[Stage1Output],
    ) -> anyhow::Result<bool> {
        self.acquire_output_and_global_job_lock(transaction).await?;
        let rows_affected = sqlx::query(AssertSqlSafe(format!(
            "WITH db_clock AS ( \
             SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint AS now \
             ) UPDATE {} AS job SET status = 'done', finished_at = db_clock.now, \
             lease_until = NULL, last_error = NULL, last_success_watermark = \
             GREATEST(COALESCE(job.last_success_watermark, 0), $1) FROM db_clock \
             WHERE job.kind = $2 AND job.job_key = $3 AND job.status = 'running' \
             AND job.ownership_token = $4 AND job.lease_until > db_clock.now",
            self.jobs_table
        )))
        .bind(completed_watermark)
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .bind(ownership_token)
        .execute(&mut **transaction)
        .await?
        .rows_affected();

        if rows_affected == 0 {
            return Ok(false);
        }

        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET selected_for_phase2 = FALSE, \
             selected_for_phase2_source_updated_at = NULL \
             WHERE selected_for_phase2 OR selected_for_phase2_source_updated_at IS NOT NULL",
            self.outputs_table
        )))
        .execute(&mut **transaction)
        .await?;

        for output in selected_outputs {
            sqlx::query(AssertSqlSafe(format!(
                "UPDATE {} SET selected_for_phase2 = TRUE, \
                 selected_for_phase2_source_updated_at = $1 \
                 WHERE thread_id = $2 AND source_updated_at = $1",
                self.outputs_table
            )))
            .bind(output.source_updated_at.timestamp())
            .bind(output.thread_id.to_string())
            .execute(&mut **transaction)
            .await?;
        }

        Ok(true)
    }

    pub(crate) async fn mark_global_phase2_job_failed(
        &self,
        ownership_token: &str,
        failure_reason: &str,
        retry_delay_seconds: i64,
    ) -> anyhow::Result<bool> {
        let rows_affected = sqlx::query(AssertSqlSafe(format!(
            "WITH db_clock AS ( \
             SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint AS now \
             ) UPDATE {} AS job SET status = 'error', finished_at = db_clock.now, \
             lease_until = NULL, retry_at = db_clock.now + $1, \
             retry_remaining = GREATEST(job.retry_remaining - 1, 0), last_error = $2 \
             FROM db_clock WHERE job.kind = $3 AND job.job_key = $4 \
             AND job.status = 'running' AND job.ownership_token = $5 \
             AND job.lease_until > db_clock.now",
            self.jobs_table
        )))
        .bind(retry_delay_seconds.max(0))
        .bind(failure_reason)
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .bind(ownership_token)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows_affected > 0)
    }

    pub(crate) async fn mark_global_phase2_job_failed_if_unowned(
        &self,
        ownership_token: &str,
        failure_reason: &str,
        retry_delay_seconds: i64,
    ) -> anyhow::Result<bool> {
        let rows_affected = sqlx::query(AssertSqlSafe(format!(
            "WITH db_clock AS ( \
             SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint AS now \
             ) UPDATE {} AS job SET status = 'error', finished_at = db_clock.now, \
             lease_until = NULL, retry_at = db_clock.now + $1, \
             retry_remaining = GREATEST(job.retry_remaining - 1, 0), last_error = $2 \
             FROM db_clock WHERE job.kind = $3 AND job.job_key = $4 \
             AND job.status = 'running' AND (job.ownership_token IS NULL OR ( \
             job.ownership_token = $5 AND (job.lease_until IS NULL \
             OR job.lease_until <= db_clock.now)))",
            self.jobs_table
        )))
        .bind(retry_delay_seconds.max(0))
        .bind(failure_reason)
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .bind(ownership_token)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows_affected > 0)
    }
}
