use crate::Stage1JobClaimOutcome;
use crate::postgres::qualified_table;
use codex_protocol::ThreadId;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use sqlx::Row;
use uuid::Uuid;

#[path = "postgres/artifacts.rs"]
mod artifacts;
#[path = "postgres/outputs.rs"]
mod outputs;
#[path = "postgres/phase2.rs"]
mod phase2;
#[path = "postgres/startup.rs"]
mod startup;

const DEFAULT_RETRY_REMAINING: i64 = 3;
const JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL: &str = "memory_consolidate_global";
const JOB_KIND_MEMORY_STAGE1: &str = "memory_stage1";
const MEMORY_CONSOLIDATION_JOB_KEY: &str = "global";

#[derive(Clone)]
pub(crate) struct PostgresMemoryStore {
    artifacts_table: String,
    generation_state_table: String,
    generations_table: String,
    history_table: String,
    jobs_table: String,
    mode_overrides_table: String,
    outputs_table: String,
    pool: PgPool,
    schema: String,
    threads_table: String,
}

impl PostgresMemoryStore {
    pub(crate) fn new(pool: PgPool, schema: String) -> Self {
        Self {
            artifacts_table: qualified_table(&schema, "memory_generation_artifacts"),
            generation_state_table: qualified_table(&schema, "memory_generation_state"),
            generations_table: qualified_table(&schema, "memory_generations"),
            history_table: qualified_table(&schema, "thread_history"),
            jobs_table: qualified_table(&schema, "memory_jobs"),
            mode_overrides_table: qualified_table(&schema, "memory_thread_mode_overrides"),
            outputs_table: qualified_table(&schema, "memory_stage1_outputs"),
            pool,
            threads_table: qualified_table(&schema, "threads"),
            schema,
        }
    }

    pub(super) async fn close(&self) {
        self.pool.close().await;
    }

    /// Serializes transactions that can lock both memory outputs and the global phase-2 job.
    ///
    /// Callers acquire this namespace-scoped lock before touching either resource. Unlike row
    /// locks, the advisory lock also orders transactions when an output or global job row has not
    /// been inserted yet.
    pub(crate) async fn acquire_output_and_global_job_lock(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended(\
             current_database() || ':codex-runtime-state:' || $1 || \
             ':memory-output-global-job', 0))",
        )
        .bind(&self.schema)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    fn enabled_thread_predicate(&self) -> String {
        let canonical_mode = format!(
            "SELECT history.item #>> '{{payload,memory_mode}}' FROM {} AS history \
             WHERE history.thread_id = thread.thread_id \
             AND history.item ->> 'type' = 'session_meta' \
             AND history.item #>> '{{payload,id}}' = thread.thread_id \
             AND history.item #>> '{{payload,memory_mode}}' IS NOT NULL \
             ORDER BY history.ordinal DESC LIMIT 1",
            self.history_table
        );
        let canonical_mode_ordinal = format!(
            "SELECT history.ordinal FROM {} AS history \
             WHERE history.thread_id = thread.thread_id \
             AND history.item ->> 'type' = 'session_meta' \
             AND history.item #>> '{{payload,id}}' = thread.thread_id \
             AND history.item #>> '{{payload,memory_mode}}' IS NOT NULL \
             ORDER BY history.ordinal DESC LIMIT 1",
            self.history_table
        );
        format!(
            "COALESCE(({canonical_mode}), 'enabled') = 'enabled' AND NOT EXISTS (\
             SELECT 1 FROM {} AS mode_override \
             WHERE mode_override.thread_id = thread.thread_id \
             AND mode_override.polluted_at_stream_version > \
             COALESCE(({canonical_mode_ordinal}), -1))",
            self.mode_overrides_table
        )
    }

    pub(super) async fn try_claim_stage1_job(
        &self,
        thread_id: ThreadId,
        worker_id: ThreadId,
        source_updated_at: i64,
        lease_seconds: i64,
        max_running_jobs: usize,
    ) -> anyhow::Result<Stage1JobClaimOutcome> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended(\
             current_database() || ':codex-runtime-state:' || $1 || ':memory-stage1-claims', 0))",
        )
        .bind(&self.schema)
        .execute(&mut *transaction)
        .await?;
        let now: i64 =
            sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
                .fetch_one(&mut *transaction)
                .await?;
        let lease_until = now.saturating_add(lease_seconds.max(0));
        let thread_id = thread_id.to_string();

        let existing_source_updated_at: Option<i64> = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT source_updated_at FROM {} WHERE thread_id = $1",
            self.outputs_table
        )))
        .bind(&thread_id)
        .fetch_optional(&mut *transaction)
        .await?;
        if existing_source_updated_at.is_some_and(|watermark| watermark >= source_updated_at) {
            transaction.commit().await?;
            return Ok(Stage1JobClaimOutcome::SkippedUpToDate);
        }

        let existing_job = sqlx::query(AssertSqlSafe(format!(
            "SELECT status, lease_until, retry_at, retry_remaining, input_watermark, \
             last_success_watermark FROM {} WHERE kind = $1 AND job_key = $2",
            self.jobs_table
        )))
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(&thread_id)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(existing_job) = &existing_job {
            let last_success_watermark: Option<i64> =
                existing_job.try_get("last_success_watermark")?;
            if last_success_watermark.is_some_and(|watermark| watermark >= source_updated_at) {
                transaction.commit().await?;
                return Ok(Stage1JobClaimOutcome::SkippedUpToDate);
            }
            let status: String = existing_job.try_get("status")?;
            let lease_until: Option<i64> = existing_job.try_get("lease_until")?;
            if status == "running" && lease_until.is_some_and(|lease_until| lease_until > now) {
                transaction.commit().await?;
                return Ok(Stage1JobClaimOutcome::SkippedRunning);
            }
            let input_watermark: Option<i64> = existing_job.try_get("input_watermark")?;
            let source_advanced =
                input_watermark.is_none_or(|watermark| source_updated_at > watermark);
            if !source_advanced {
                let retry_remaining: i64 = existing_job.try_get("retry_remaining")?;
                if retry_remaining <= 0 {
                    transaction.commit().await?;
                    return Ok(Stage1JobClaimOutcome::SkippedRetryExhausted);
                }
                let retry_at: Option<i64> = existing_job.try_get("retry_at")?;
                if retry_at.is_some_and(|retry_at| retry_at > now) {
                    transaction.commit().await?;
                    return Ok(Stage1JobClaimOutcome::SkippedRetryBackoff);
                }
            }
        }

        let running_jobs: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {} WHERE kind = $1 AND status = 'running' \
             AND lease_until IS NOT NULL AND lease_until > $2 AND job_key != $3",
            self.jobs_table
        )))
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(now)
        .bind(&thread_id)
        .fetch_one(&mut *transaction)
        .await?;
        let max_running_jobs = i64::try_from(max_running_jobs).unwrap_or(i64::MAX);
        if running_jobs >= max_running_jobs {
            transaction.commit().await?;
            return Ok(Stage1JobClaimOutcome::SkippedRunning);
        }

        let ownership_token = Uuid::new_v4().to_string();
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} AS current (kind, job_key, thread_id, status, worker_id, ownership_token, \
             started_at, finished_at, lease_until, retry_at, retry_remaining, last_error, \
             input_watermark, last_success_watermark) \
             VALUES ($1, $2, $2, 'running', $3, $4, $5, NULL, $6, NULL, $7, NULL, $8, NULL) \
             ON CONFLICT(kind, job_key) DO UPDATE SET status = 'running', \
             thread_id = excluded.thread_id, worker_id = excluded.worker_id, \
             ownership_token = excluded.ownership_token, \
             started_at = excluded.started_at, finished_at = NULL, \
             lease_until = excluded.lease_until, retry_at = NULL, \
             retry_remaining = CASE WHEN excluded.input_watermark > \
             COALESCE(current.input_watermark, -1) THEN $7 ELSE current.retry_remaining END, \
             last_error = NULL, input_watermark = excluded.input_watermark",
            self.jobs_table
        )))
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(&thread_id)
        .bind(worker_id.to_string())
        .bind(&ownership_token)
        .bind(now)
        .bind(lease_until)
        .bind(DEFAULT_RETRY_REMAINING)
        .bind(source_updated_at)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(Stage1JobClaimOutcome::Claimed { ownership_token })
    }

    pub(super) async fn mark_stage1_job_succeeded(
        &self,
        thread_id: ThreadId,
        ownership_token: &str,
        source_updated_at: i64,
        raw_memory: &str,
        rollout_summary: &str,
        rollout_slug: Option<&str>,
    ) -> anyhow::Result<bool> {
        let mut transaction = self.pool.begin().await?;
        self.acquire_output_and_global_job_lock(&mut transaction)
            .await?;
        let now: i64 =
            sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
                .fetch_one(&mut *transaction)
                .await?;
        let thread_id = thread_id.to_string();
        let completed_input: Option<i64> = sqlx::query_scalar(AssertSqlSafe(format!(
            "UPDATE {} SET status = 'done', finished_at = $1, lease_until = NULL, \
             last_error = NULL, last_success_watermark = input_watermark \
             WHERE kind = $2 AND job_key = $3 AND status = 'running' AND ownership_token = $4 \
             AND lease_until > FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint \
             RETURNING input_watermark",
            self.jobs_table
        )))
        .bind(now)
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(&thread_id)
        .bind(ownership_token)
        .fetch_optional(&mut *transaction)
        .await?;
        if completed_input.is_none() {
            transaction.commit().await?;
            return Ok(false);
        }

        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} AS current (thread_id, source_updated_at, raw_memory, \
             rollout_summary, rollout_slug, generated_at) VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT(thread_id) DO UPDATE SET source_updated_at = excluded.source_updated_at, \
             raw_memory = excluded.raw_memory, rollout_summary = excluded.rollout_summary, \
             rollout_slug = excluded.rollout_slug, generated_at = excluded.generated_at \
             WHERE excluded.source_updated_at >= current.source_updated_at",
            self.outputs_table
        )))
        .bind(&thread_id)
        .bind(source_updated_at)
        .bind(raw_memory)
        .bind(rollout_summary)
        .bind(rollout_slug)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        self.enqueue_global_consolidation_in_transaction(&mut transaction, source_updated_at)
            .await?;
        transaction.commit().await?;
        Ok(true)
    }

    pub(super) async fn mark_stage1_job_succeeded_no_output(
        &self,
        thread_id: ThreadId,
        ownership_token: &str,
    ) -> anyhow::Result<bool> {
        let mut transaction = self.pool.begin().await?;
        self.acquire_output_and_global_job_lock(&mut transaction)
            .await?;
        let thread_id = thread_id.to_string();
        let source_updated_at: Option<i64> = sqlx::query_scalar(AssertSqlSafe(format!(
            "UPDATE {} SET status = 'done', \
             finished_at = FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint, \
             lease_until = NULL, last_error = NULL, last_success_watermark = input_watermark \
             WHERE kind = $1 AND job_key = $2 AND status = 'running' AND ownership_token = $3 \
             AND lease_until > FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint \
             RETURNING input_watermark",
            self.jobs_table
        )))
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(&thread_id)
        .bind(ownership_token)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(source_updated_at) = source_updated_at else {
            transaction.commit().await?;
            return Ok(false);
        };
        let deleted_rows = sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM {} WHERE thread_id = $1",
            self.outputs_table
        )))
        .bind(&thread_id)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if deleted_rows > 0 {
            self.enqueue_global_consolidation_in_transaction(&mut transaction, source_updated_at)
                .await?;
        }
        transaction.commit().await?;
        Ok(true)
    }

    pub(super) async fn heartbeat_stage1_job(
        &self,
        thread_id: ThreadId,
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
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(thread_id.to_string())
        .bind(ownership_token)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows_affected > 0)
    }

    pub(super) async fn mark_stage1_job_failed(
        &self,
        thread_id: ThreadId,
        ownership_token: &str,
        failure_reason: &str,
        retry_delay_seconds: i64,
    ) -> anyhow::Result<bool> {
        let rows_affected = sqlx::query(AssertSqlSafe(format!(
            "WITH db_clock AS ( \
             SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint AS now \
             ) UPDATE {} AS job SET status = 'error', finished_at = db_clock.now, \
             lease_until = NULL, retry_at = db_clock.now + $1, \
             retry_remaining = job.retry_remaining - 1, last_error = $2 FROM db_clock \
             WHERE job.kind = $3 AND job.job_key = $4 AND job.status = 'running' \
             AND job.ownership_token = $5 AND job.lease_until > db_clock.now",
            self.jobs_table
        )))
        .bind(retry_delay_seconds.max(0))
        .bind(failure_reason)
        .bind(JOB_KIND_MEMORY_STAGE1)
        .bind(thread_id.to_string())
        .bind(ownership_token)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows_affected > 0)
    }

    pub(crate) async fn enqueue_global_consolidation_in_transaction(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        input_watermark: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} AS current (kind, job_key, thread_id, status, worker_id, ownership_token, \
             started_at, finished_at, lease_until, retry_at, retry_remaining, last_error, \
             input_watermark, last_success_watermark) \
             VALUES ($1, $2, NULL, 'pending', NULL, NULL, NULL, NULL, NULL, NULL, $3, NULL, $4, 0) \
             ON CONFLICT(kind, job_key) DO UPDATE SET \
             status = CASE WHEN current.status = 'running' THEN 'running' ELSE 'pending' END, \
             retry_at = CASE WHEN current.status = 'running' THEN current.retry_at ELSE NULL END, \
             retry_remaining = GREATEST(current.retry_remaining, excluded.retry_remaining), \
             input_watermark = CASE WHEN excluded.input_watermark > \
             COALESCE(current.input_watermark, 0) THEN excluded.input_watermark \
             ELSE COALESCE(current.input_watermark, 0) + 1 END",
            self.jobs_table
        )))
        .bind(JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .bind(MEMORY_CONSOLIDATION_JOB_KEY)
        .bind(DEFAULT_RETRY_REMAINING)
        .bind(input_watermark)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }
}
