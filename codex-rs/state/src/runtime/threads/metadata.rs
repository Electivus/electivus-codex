//! SQLite thread metadata reads, direct field updates, and timestamp allocation.

use super::postgres;
use super::*;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

/// Runtime metadata needed before a persisted thread is resumed or forked.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
pub struct ThreadResumeMetadata {
    /// Working directory captured for the thread.
    pub cwd: PathBuf,
    /// Latest observed model, if one has been persisted.
    pub model: Option<String>,
}

impl StateRuntime {
    /// Read the canonical working directory and model for a persisted thread.
    pub async fn get_thread_resume_metadata(
        &self,
        id: ThreadId,
    ) -> anyhow::Result<Option<ThreadResumeMetadata>> {
        if let Some((pool, schema)) = self.postgres_connection() {
            return postgres::get_thread_resume_metadata(&pool, &schema, id).await;
        }

        Ok(self
            .get_thread(id)
            .await?
            .map(|metadata| ThreadResumeMetadata {
                cwd: metadata.cwd,
                model: metadata.model,
            }))
    }

    pub async fn get_thread(&self, id: ThreadId) -> anyhow::Result<Option<crate::ThreadMetadata>> {
        let row = sqlx::query(
            r#"
SELECT
    threads.id,
    threads.rollout_path,
    threads.created_at_ms AS created_at,
    threads.updated_at_ms AS updated_at,
    threads.recency_at_ms AS recency_at,
    threads.source,
    threads.history_mode,
    threads.thread_source,
    threads.agent_nickname,
    threads.agent_role,
    threads.agent_path,
    threads.model_provider,
    threads.model,
    threads.reasoning_effort,
    threads.cwd,
    threads.cli_version,
    threads.title,
    threads.name,
    threads.preview,
    threads.sandbox_policy,
    threads.approval_mode,
    threads.tokens_used,
    threads.first_user_message,
    threads.archived_at,
    threads.is_pinned,
    threads.git_sha,
    threads.git_branch,
    threads.git_origin_url
FROM threads
WHERE threads.id = ?
            "#,
        )
        .bind(id.to_string())
        .fetch_optional(self.sqlite_pool()?)
        .await?;
        row.map(|row| ThreadRow::try_from_row(&row).and_then(ThreadMetadata::try_from))
            .transpose()
    }

    pub async fn get_thread_memory_mode(&self, id: ThreadId) -> anyhow::Result<Option<String>> {
        let row = sqlx::query("SELECT memory_mode FROM threads WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(self.sqlite_pool()?)
            .await?;
        Ok(row.and_then(|row| row.try_get("memory_mode").ok()))
    }

    pub async fn set_thread_preview_if_empty(
        &self,
        thread_id: ThreadId,
        preview: &str,
    ) -> anyhow::Result<bool> {
        let preview = preview.trim();
        if preview.is_empty() {
            return Ok(false);
        }
        if let Some((pool, schema)) = self.postgres_connection() {
            return postgres::set_thread_preview_if_empty(&pool, &schema, thread_id, preview).await;
        }

        let result = sqlx::query(
            r#"
UPDATE threads
SET preview = ?
WHERE id = ? AND preview = ''
            "#,
        )
        .bind(preview)
        .bind(thread_id.to_string())
        .execute(self.sqlite_pool()?)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Insert or replace thread metadata directly.
    pub async fn upsert_thread(&self, metadata: &crate::ThreadMetadata) -> anyhow::Result<()> {
        self.upsert_thread_with_creation_memory_mode(metadata, /*creation_memory_mode*/ None)
            .await
    }

    pub async fn insert_thread_if_absent(
        &self,
        metadata: &crate::ThreadMetadata,
    ) -> anyhow::Result<bool> {
        let updated_at = self.allocate_thread_updated_at(metadata.updated_at)?;
        let recency_at = self.allocate_thread_recency_at(metadata.recency_at)?;
        let preview = metadata_preview(metadata);
        let result = sqlx::query(
            r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    recency_at,
    created_at_ms,
    updated_at_ms,
    recency_at_ms,
    source,
    history_mode,
    thread_source,
    agent_nickname,
    agent_role,
    agent_path,
    model_provider,
    model,
    reasoning_effort,
    cwd,
    cli_version,
    title,
    name,
    preview,
    sandbox_policy,
    approval_mode,
    tokens_used,
    first_user_message,
    archived,
    archived_at,
    is_pinned,
    git_sha,
    git_branch,
    git_origin_url,
    memory_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(id) DO NOTHING
            "#,
        )
        .bind(metadata.id.to_string())
        .bind(metadata.rollout_path.display().to_string())
        .bind(datetime_to_epoch_seconds(metadata.created_at))
        .bind(datetime_to_epoch_seconds(updated_at))
        .bind(datetime_to_epoch_seconds(recency_at))
        .bind(datetime_to_epoch_millis(metadata.created_at))
        .bind(datetime_to_epoch_millis(updated_at))
        .bind(datetime_to_epoch_millis(recency_at))
        .bind(metadata.source.as_str())
        .bind(metadata.history_mode.as_str())
        .bind(
            metadata
                .thread_source
                .as_ref()
                .map(codex_protocol::protocol::ThreadSource::as_str),
        )
        .bind(metadata.agent_nickname.as_deref())
        .bind(metadata.agent_role.as_deref())
        .bind(metadata.agent_path.as_deref())
        .bind(metadata.model_provider.as_str())
        .bind(metadata.model.as_deref())
        .bind(
            metadata
                .reasoning_effort
                .as_ref()
                .map(crate::extract::enum_to_string),
        )
        .bind(metadata.cwd.display().to_string())
        .bind(metadata.cli_version.as_str())
        .bind(metadata.title.as_str())
        .bind(metadata.name.as_deref())
        .bind(preview)
        .bind(metadata.sandbox_policy.as_str())
        .bind(metadata.approval_mode.as_str())
        .bind(metadata.tokens_used)
        .bind(metadata.first_user_message.as_deref().unwrap_or_default())
        .bind(metadata.archived_at.is_some())
        .bind(metadata.archived_at.map(datetime_to_epoch_seconds))
        .bind(metadata.is_pinned)
        .bind(metadata.git_sha.as_deref())
        .bind(metadata.git_branch.as_deref())
        .bind(metadata.git_origin_url.as_deref())
        .bind("enabled")
        .execute(self.sqlite_pool()?)
        .await?;
        self.insert_thread_spawn_edge_from_source_if_absent(metadata.id, metadata.source.as_str())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Update pinned state without changing rollout-derived metadata.
    pub async fn update_thread_pin(
        &self,
        thread_id: ThreadId,
        is_pinned: bool,
    ) -> anyhow::Result<bool> {
        if let Some((pool, schema)) = self.postgres_connection() {
            return postgres::update_thread_pin(&pool, &schema, thread_id, is_pinned).await;
        }
        let result = sqlx::query("UPDATE threads SET is_pinned = ? WHERE id = ?")
            .bind(is_pinned)
            .bind(thread_id.to_string())
            .execute(self.sqlite_pool()?)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn set_thread_memory_mode(
        &self,
        thread_id: ThreadId,
        memory_mode: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query("UPDATE threads SET memory_mode = ? WHERE id = ?")
            .bind(memory_mode)
            .bind(thread_id.to_string())
            .execute(self.sqlite_pool()?)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn update_thread_title(
        &self,
        thread_id: ThreadId,
        title: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query("UPDATE threads SET title = ? WHERE id = ?")
            .bind(title)
            .bind(thread_id.to_string())
            .execute(self.sqlite_pool()?)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn update_thread_name(
        &self,
        thread_id: ThreadId,
        name: Option<&str>,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query("UPDATE threads SET name = ? WHERE id = ?")
            .bind(name)
            .bind(thread_id.to_string())
            .execute(self.sqlite_pool()?)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn touch_thread_updated_at(
        &self,
        thread_id: ThreadId,
        updated_at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let updated_at = self.allocate_thread_updated_at(updated_at)?;
        let result =
            sqlx::query("UPDATE threads SET updated_at = ?, updated_at_ms = ? WHERE id = ?")
                .bind(datetime_to_epoch_seconds(updated_at))
                .bind(datetime_to_epoch_millis(updated_at))
                .bind(thread_id.to_string())
                .execute(self.sqlite_pool()?)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn touch_thread_recency_at(
        &self,
        thread_id: ThreadId,
        recency_at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let recency_at = self.allocate_thread_recency_at(recency_at)?;
        let recency_at_seconds = datetime_to_epoch_seconds(recency_at);
        let recency_at_millis = datetime_to_epoch_millis(recency_at);
        let result = sqlx::query(
            r#"
UPDATE threads
SET
    recency_at = MAX(?, MAX(?, recency_at_ms + 1) / 1000),
    recency_at_ms = MAX(?, recency_at_ms + 1)
WHERE id = ?
            "#,
        )
        .bind(recency_at_seconds)
        .bind(recency_at_millis)
        .bind(recency_at_millis)
        .bind(thread_id.to_string())
        .execute(self.sqlite_pool()?)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Allocate a persisted `updated_at` value for thread-list cursor ordering.
    ///
    /// We keep a process-local high-water mark so hot rollout writes can get unique,
    /// monotonic millisecond timestamps without querying SQLite on every update. Older
    /// backfill/repair timestamps are allowed through unchanged so historical ordering
    /// remains tied to the rollout file mtimes.
    pub(super) fn allocate_thread_updated_at(
        &self,
        updated_at: DateTime<Utc>,
    ) -> anyhow::Result<DateTime<Utc>> {
        allocate_thread_timestamp(self.thread_updated_at_millis.as_ref(), updated_at)
    }

    pub(super) fn allocate_thread_recency_at(
        &self,
        recency_at: DateTime<Utc>,
    ) -> anyhow::Result<DateTime<Utc>> {
        allocate_thread_timestamp(self.thread_recency_at_millis.as_ref(), recency_at)
    }
}

fn allocate_thread_timestamp(
    high_water_mark: &AtomicI64,
    timestamp: DateTime<Utc>,
) -> anyhow::Result<DateTime<Utc>> {
    let candidate = datetime_to_epoch_millis(timestamp);
    let allocated = loop {
        let current = high_water_mark.load(Ordering::Relaxed);

        // New wall-clock time: advance the process-local high-water mark and use it as-is.
        if candidate > current {
            if high_water_mark
                .compare_exchange(current, candidate, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break candidate;
            }
            continue;
        }

        // Older timestamps come from backfill/repair paths that preserve rollout mtimes.
        // Do not drag historical rows forward just because this process has seen newer writes.
        if candidate.saturating_add(1000) <= current {
            break candidate;
        }

        // Same hot one-second bucket as the current high-water mark. Allocate the next
        // millisecond so the timestamp remains unique and cursor-orderable inside the process.
        let bumped = current.saturating_add(1);
        if high_water_mark
            .compare_exchange(current, bumped, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break bumped;
        }
    };
    epoch_millis_to_datetime(allocated)
}

pub(super) fn metadata_preview(metadata: &crate::ThreadMetadata) -> &str {
    metadata
        .preview
        .as_deref()
        .or(metadata.first_user_message.as_deref())
        .unwrap_or_default()
}
