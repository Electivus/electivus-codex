use super::PostgresMemoryStore;
use crate::Stage1JobClaim;
use crate::Stage1JobClaimOutcome;
use crate::Stage1StartupClaimParams;
use crate::ThreadMetadata;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::GitInfo;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TokenUsage;
use serde::Deserialize;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Row;
use std::path::PathBuf;

#[derive(Deserialize)]
struct PostgresThreadProjection {
    thread_id: ThreadId,
    rollout_path: Option<PathBuf>,
    preview: String,
    name: Option<String>,
    model_provider: String,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    recency_at: DateTime<Utc>,
    archived_at: Option<DateTime<Utc>>,
    #[serde(default)]
    is_pinned: bool,
    cwd: PathBuf,
    cli_version: String,
    source: SessionSource,
    history_mode: ThreadHistoryMode,
    thread_source: Option<ThreadSource>,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
    agent_path: Option<String>,
    git_info: Option<GitInfo>,
    approval_mode: AskForApproval,
    permission_profile: PermissionProfile,
    token_usage: Option<TokenUsage>,
    first_user_message: Option<String>,
}

impl PostgresMemoryStore {
    pub(crate) async fn clear_memory_data(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        self.acquire_output_and_global_job_lock(&mut transaction)
            .await?;
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET active_generation_id = NULL WHERE singleton",
            self.generation_state_table
        )))
        .execute(&mut *transaction)
        .await?;
        sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM {}",
            self.artifacts_table
        )))
        .execute(&mut *transaction)
        .await?;
        sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM {}",
            self.generations_table
        )))
        .execute(&mut *transaction)
        .await?;
        sqlx::query(AssertSqlSafe(format!("DELETE FROM {}", self.outputs_table)))
            .execute(&mut *transaction)
            .await?;
        sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM {} WHERE kind = $1 OR kind = $2",
            self.jobs_table
        )))
        .bind(super::JOB_KIND_MEMORY_STAGE1)
        .bind(super::JOB_KIND_MEMORY_CONSOLIDATE_GLOBAL)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn delete_thread_memory(&self, thread_id: ThreadId) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        self.acquire_output_and_global_job_lock(&mut transaction)
            .await?;
        let thread_id = thread_id.to_string();
        let selected_for_phase2: Option<bool> = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT selected_for_phase2 FROM {} WHERE thread_id = $1 FOR UPDATE",
            self.outputs_table
        )))
        .bind(&thread_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let deleted_rows = sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM {} WHERE thread_id = $1",
            self.outputs_table
        )))
        .bind(&thread_id)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM {} WHERE kind = $1 AND job_key = $2",
            self.jobs_table
        )))
        .bind(super::JOB_KIND_MEMORY_STAGE1)
        .bind(&thread_id)
        .execute(&mut *transaction)
        .await?;
        if deleted_rows > 0 && selected_for_phase2 == Some(true) {
            let now: i64 =
                sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
                    .fetch_one(&mut *transaction)
                    .await?;
            self.enqueue_global_consolidation_in_transaction(&mut transaction, now)
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn claim_stage1_jobs_for_startup(
        &self,
        current_thread_id: ThreadId,
        params: Stage1StartupClaimParams<'_>,
    ) -> anyhow::Result<Vec<Stage1JobClaim>> {
        let Stage1StartupClaimParams {
            scan_limit,
            max_claimed,
            max_age_days,
            min_rollout_idle_hours,
            allowed_sources,
            lease_seconds,
        } = params;
        if scan_limit == 0 || max_claimed == 0 {
            return Ok(Vec::new());
        }
        let enabled_thread = self.enabled_thread_predicate();
        let rows = sqlx::query(AssertSqlSafe(format!(
            "WITH db_clock AS (SELECT clock_timestamp() AS now) \
             SELECT thread.projection FROM {} AS thread CROSS JOIN db_clock \
             WHERE thread.archived_at IS NULL \
             AND COALESCE(thread.projection ->> 'preview', '') <> '' \
             AND (cardinality($1::text[]) = 0 OR thread.projection ->> 'source' = ANY($1)) \
             AND thread.thread_id != $2 \
             AND thread.updated_at >= db_clock.now - $3 * INTERVAL '1 second' \
             AND thread.updated_at <= db_clock.now - $4 * INTERVAL '1 second' \
             AND {enabled_thread} ORDER BY thread.updated_at DESC LIMIT $5",
            self.threads_table
        )))
        .bind(allowed_sources)
        .bind(current_thread_id.to_string())
        .bind(max_age_days.max(0).saturating_mul(24 * 60 * 60))
        .bind(min_rollout_idle_hours.max(0).saturating_mul(60 * 60))
        .bind(i64::try_from(scan_limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        let candidates = rows
            .into_iter()
            .map(|row| {
                let projection: Value = row.try_get("projection")?;
                let projection = serde_json::from_value(projection)?;
                thread_metadata_from_projection(projection)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let mut claimed = Vec::new();
        for thread in candidates {
            if claimed.len() >= max_claimed {
                break;
            }
            if let Stage1JobClaimOutcome::Claimed { ownership_token } = self
                .try_claim_stage1_job(
                    thread.id,
                    current_thread_id,
                    thread.updated_at.timestamp(),
                    lease_seconds,
                    max_claimed,
                )
                .await?
            {
                claimed.push(Stage1JobClaim {
                    thread,
                    ownership_token,
                });
            }
        }
        Ok(claimed)
    }

    /// Records runtime-detected pollution without rewriting Canonical Thread History.
    ///
    /// The stored stream version makes the override temporary: an explicit memory-mode entry
    /// subsequently appended by the ThreadStore has an ordinal at or above this version and wins.
    pub(crate) async fn mark_thread_memory_mode_polluted(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<bool> {
        let mut transaction = self.pool.begin().await?;
        self.acquire_output_and_global_job_lock(&mut transaction)
            .await?;
        let thread_id = thread_id.to_string();
        let stream_version: Option<i64> = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT stream_version FROM {} WHERE thread_id = $1 FOR UPDATE",
            self.threads_table
        )))
        .bind(&thread_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(stream_version) = stream_version else {
            transaction.commit().await?;
            return Ok(false);
        };

        let canonical_mode = sqlx::query(AssertSqlSafe(format!(
            "SELECT item #>> '{{payload,memory_mode}}' AS memory_mode, ordinal FROM {} \
             WHERE thread_id = $1 AND item ->> 'type' = 'session_meta' \
             AND item #>> '{{payload,id}}' = $1 \
             AND item #>> '{{payload,memory_mode}}' IS NOT NULL \
             ORDER BY ordinal DESC LIMIT 1",
            self.history_table
        )))
        .bind(&thread_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let (canonical_mode, canonical_ordinal) = match canonical_mode {
            Some(row) => (
                row.try_get::<String, _>("memory_mode")?,
                row.try_get::<i64, _>("ordinal")?,
            ),
            None => ("enabled".to_string(), -1),
        };
        let existing_override: Option<i64> = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT polluted_at_stream_version FROM {} WHERE thread_id = $1",
            self.mode_overrides_table
        )))
        .bind(&thread_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let override_is_effective =
            existing_override.is_some_and(|version| version > canonical_ordinal);
        let changed = canonical_mode != "polluted" && !override_is_effective;
        if changed {
            sqlx::query(AssertSqlSafe(format!(
                "INSERT INTO {} (thread_id, polluted_at_stream_version) VALUES ($1, $2) \
                 ON CONFLICT(thread_id) DO UPDATE SET \
                 polluted_at_stream_version = excluded.polluted_at_stream_version",
                self.mode_overrides_table
            )))
            .bind(&thread_id)
            .bind(stream_version)
            .execute(&mut *transaction)
            .await?;
        }

        let selected_for_phase2: bool = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT selected_for_phase2 FROM {} WHERE thread_id = $1",
            self.outputs_table
        )))
        .bind(&thread_id)
        .fetch_optional(&mut *transaction)
        .await?
        .unwrap_or(false);
        if selected_for_phase2 {
            let now: i64 =
                sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
                    .fetch_one(&mut *transaction)
                    .await?;
            self.enqueue_global_consolidation_in_transaction(&mut transaction, now)
                .await?;
        }
        transaction.commit().await?;
        Ok(changed)
    }
}

fn thread_metadata_from_projection(
    projection: PostgresThreadProjection,
) -> anyhow::Result<ThreadMetadata> {
    let (git_sha, git_branch, git_origin_url) = match projection.git_info {
        Some(git_info) => (
            git_info.commit_hash.map(|sha| sha.0),
            git_info.branch,
            git_info.repository_url,
        ),
        None => (None, None, None),
    };
    let (title, name) = match projection.history_mode {
        ThreadHistoryMode::Legacy => (
            projection
                .name
                .clone()
                .unwrap_or_else(|| projection.preview.clone()),
            None,
        ),
        ThreadHistoryMode::Paginated => (projection.preview.clone(), projection.name),
    };
    Ok(ThreadMetadata {
        id: projection.thread_id,
        rollout_path: projection.rollout_path.unwrap_or_default(),
        created_at: projection.created_at,
        updated_at: projection.updated_at,
        recency_at: projection.recency_at,
        source: crate::extract::enum_to_string(&projection.source),
        history_mode: projection.history_mode,
        thread_source: projection.thread_source,
        agent_nickname: projection.agent_nickname,
        agent_role: projection.agent_role,
        agent_path: projection.agent_path,
        model_provider: projection.model_provider,
        model: projection.model,
        reasoning_effort: projection.reasoning_effort,
        cwd: projection.cwd,
        cli_version: projection.cli_version,
        title,
        name,
        preview: Some(projection.preview),
        sandbox_policy: serde_json::to_string(&projection.permission_profile)?,
        approval_mode: crate::extract::enum_to_string(&projection.approval_mode),
        tokens_used: projection
            .token_usage
            .map_or(0, |usage| usage.total_tokens.max(0)),
        first_user_message: projection.first_user_message,
        archived_at: projection.archived_at,
        is_pinned: projection.is_pinned,
        git_sha,
        git_branch,
        git_origin_url,
    })
}
