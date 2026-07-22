use super::PostgresMemoryStore;
use crate::Stage1Output;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::AssertSqlSafe;
use sqlx::Row;
use sqlx::postgres::PgRow;
use std::path::PathBuf;

impl PostgresMemoryStore {
    pub(crate) async fn record_stage1_output_usage(
        &self,
        thread_ids: &[ThreadId],
    ) -> anyhow::Result<usize> {
        if thread_ids.is_empty() {
            return Ok(0);
        }
        let thread_ids = thread_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let updated_occurrences: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
            "WITH requested AS (SELECT unnest($1::text[]) AS thread_id), \
             increments AS (SELECT thread_id, COUNT(*)::bigint AS increment \
             FROM requested GROUP BY thread_id), \
             db_clock AS (SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint AS now), \
             updated AS (UPDATE {} AS output SET \
             usage_count = COALESCE(output.usage_count, 0) + increments.increment, \
             last_usage = db_clock.now FROM increments CROSS JOIN db_clock \
             WHERE output.thread_id = increments.thread_id RETURNING increments.increment) \
             SELECT COALESCE(SUM(increment), 0)::bigint FROM updated",
            self.outputs_table
        )))
        .bind(thread_ids)
        .fetch_one(&self.pool)
        .await?;
        Ok(usize::try_from(updated_occurrences).unwrap_or(usize::MAX))
    }

    pub(crate) async fn list_stage1_outputs_for_global(
        &self,
        n: usize,
    ) -> anyhow::Result<Vec<Stage1Output>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let enabled_thread = self.enabled_thread_predicate();
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT output.thread_id, output.source_updated_at, output.raw_memory, \
             output.rollout_summary, output.rollout_slug, output.generated_at, \
             COALESCE(thread.projection ->> 'rollout_path', '') AS rollout_path, \
             COALESCE(thread.projection ->> 'cwd', '') AS cwd, \
             thread.projection #>> '{{git_info,branch}}' AS git_branch \
             FROM {} AS output JOIN {} AS thread ON thread.thread_id = output.thread_id \
             WHERE (length(trim(output.raw_memory)) > 0 \
             OR length(trim(output.rollout_summary)) > 0) AND {enabled_thread} \
             ORDER BY output.source_updated_at DESC, output.thread_id DESC LIMIT $1",
            self.outputs_table, self.threads_table
        )))
        .bind(i64::try_from(n).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(stage1_output_from_row).collect()
    }

    pub(crate) async fn get_phase2_input_selection(
        &self,
        n: usize,
        max_unused_days: i64,
    ) -> anyhow::Result<Vec<Stage1Output>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let enabled_thread = self.enabled_thread_predicate();
        let rows = sqlx::query(AssertSqlSafe(format!(
            "WITH db_clock AS ( \
             SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint AS now \
             ), ranked AS (SELECT output.thread_id, output.source_updated_at, \
             output.raw_memory, output.rollout_summary, output.rollout_slug, \
             output.generated_at, COALESCE(thread.projection ->> 'rollout_path', '') \
             AS rollout_path, COALESCE(thread.projection ->> 'cwd', '') AS cwd, \
             thread.projection #>> '{{git_info,branch}}' AS git_branch \
             FROM {} AS output JOIN {} AS thread ON thread.thread_id = output.thread_id \
             CROSS JOIN db_clock WHERE (length(trim(output.raw_memory)) > 0 \
             OR length(trim(output.rollout_summary)) > 0) AND {enabled_thread} \
             AND ((output.last_usage IS NOT NULL \
             AND output.last_usage >= db_clock.now - $1) \
             OR (output.last_usage IS NULL \
             AND output.source_updated_at >= db_clock.now - $1)) \
             ORDER BY COALESCE(output.usage_count, 0) DESC, \
             COALESCE(output.last_usage, output.source_updated_at) DESC, \
             output.source_updated_at DESC, output.thread_id DESC LIMIT $2) \
             SELECT * FROM ranked ORDER BY thread_id ASC",
            self.outputs_table, self.threads_table
        )))
        .bind(retention_seconds(max_unused_days))
        .bind(i64::try_from(n).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(stage1_output_from_row).collect()
    }

    pub(crate) async fn prune_stage1_outputs_for_retention(
        &self,
        max_unused_days: i64,
        limit: usize,
    ) -> anyhow::Result<usize> {
        if limit == 0 {
            return Ok(0);
        }
        let rows_affected = sqlx::query(AssertSqlSafe(format!(
            "WITH db_clock AS ( \
             SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint AS now \
             ), victims AS (SELECT output.thread_id FROM {} AS output CROSS JOIN db_clock \
             WHERE output.selected_for_phase2 = FALSE \
             AND COALESCE(output.last_usage, output.source_updated_at) < db_clock.now - $1 \
             ORDER BY COALESCE(output.last_usage, output.source_updated_at) ASC, \
             output.source_updated_at ASC, output.thread_id ASC LIMIT $2 \
             FOR UPDATE SKIP LOCKED) DELETE FROM {} AS output USING victims \
             WHERE output.thread_id = victims.thread_id",
            self.outputs_table, self.outputs_table
        )))
        .bind(retention_seconds(max_unused_days))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(usize::try_from(rows_affected).unwrap_or(usize::MAX))
    }
}

fn retention_seconds(max_unused_days: i64) -> i64 {
    max_unused_days.max(0).saturating_mul(24 * 60 * 60)
}

fn stage1_output_from_row(row: &PgRow) -> anyhow::Result<Stage1Output> {
    Ok(Stage1Output {
        thread_id: ThreadId::try_from(row.try_get::<String, _>("thread_id")?.as_str())?,
        rollout_path: PathBuf::from(row.try_get::<String, _>("rollout_path")?),
        source_updated_at: datetime_from_epoch_seconds(row.try_get("source_updated_at")?)?,
        raw_memory: row.try_get("raw_memory")?,
        rollout_summary: row.try_get("rollout_summary")?,
        rollout_slug: row.try_get("rollout_slug")?,
        cwd: PathBuf::from(row.try_get::<String, _>("cwd")?),
        git_branch: row.try_get("git_branch")?,
        generated_at: datetime_from_epoch_seconds(row.try_get("generated_at")?)?,
    })
}

fn datetime_from_epoch_seconds(seconds: i64) -> anyhow::Result<DateTime<Utc>> {
    DateTime::from_timestamp(seconds, /*nsecs*/ 0)
        .ok_or_else(|| anyhow::anyhow!("invalid unix timestamp: {seconds}"))
}
