//! SQLite rollout-derived metadata persistence and archive lifecycle updates.

use super::metadata::metadata_preview;
use super::*;

impl StateRuntime {
    pub async fn update_thread_git_info(
        &self,
        thread_id: ThreadId,
        git_sha: Option<Option<&str>>,
        git_branch: Option<Option<&str>>,
        git_origin_url: Option<Option<&str>>,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            r#"
UPDATE threads
SET
    git_sha = CASE WHEN ? THEN ? ELSE git_sha END,
    git_branch = CASE WHEN ? THEN ? ELSE git_branch END,
    git_origin_url = CASE WHEN ? THEN ? ELSE git_origin_url END
WHERE id = ?
            "#,
        )
        .bind(git_sha.is_some())
        .bind(git_sha.flatten())
        .bind(git_branch.is_some())
        .bind(git_branch.flatten())
        .bind(git_origin_url.is_some())
        .bind(git_origin_url.flatten())
        .bind(thread_id.to_string())
        .execute(self.sqlite_pool()?)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub(super) async fn upsert_thread_with_creation_memory_mode(
        &self,
        metadata: &crate::ThreadMetadata,
        creation_memory_mode: Option<&str>,
    ) -> anyhow::Result<()> {
        let updated_at = self.allocate_thread_updated_at(metadata.updated_at)?;
        let insert_recency_at = self.allocate_thread_recency_at(metadata.recency_at)?;
        let preview = metadata_preview(metadata);
        // Backfill/reconcile callers merge existing git info before upserting, but that
        // read/modify/write is not atomic. Preserve non-null SQLite git fields here so
        // an explicit metadata update cannot be lost if a stale rollout upsert lands later.
        sqlx::query(
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
ON CONFLICT(id) DO UPDATE SET
    rollout_path = excluded.rollout_path,
    created_at = excluded.created_at,
    updated_at = excluded.updated_at,
    recency_at = threads.recency_at,
    created_at_ms = excluded.created_at_ms,
    updated_at_ms = excluded.updated_at_ms,
    recency_at_ms = threads.recency_at_ms,
    source = excluded.source,
    history_mode = excluded.history_mode,
    thread_source = excluded.thread_source,
    agent_nickname = excluded.agent_nickname,
    agent_role = excluded.agent_role,
    agent_path = excluded.agent_path,
    model_provider = excluded.model_provider,
    model = excluded.model,
    reasoning_effort = excluded.reasoning_effort,
    cwd = excluded.cwd,
    cli_version = excluded.cli_version,
    title = excluded.title,
    preview = COALESCE(NULLIF(excluded.preview, ''), threads.preview),
    sandbox_policy = excluded.sandbox_policy,
    approval_mode = excluded.approval_mode,
    tokens_used = excluded.tokens_used,
    first_user_message = excluded.first_user_message,
    archived = excluded.archived,
    archived_at = excluded.archived_at,
    git_sha = COALESCE(threads.git_sha, excluded.git_sha),
    git_branch = COALESCE(threads.git_branch, excluded.git_branch),
    git_origin_url = COALESCE(threads.git_origin_url, excluded.git_origin_url)
            "#,
        )
        .bind(metadata.id.to_string())
        .bind(metadata.rollout_path.display().to_string())
        .bind(datetime_to_epoch_seconds(metadata.created_at))
        .bind(datetime_to_epoch_seconds(updated_at))
        .bind(datetime_to_epoch_seconds(insert_recency_at))
        .bind(datetime_to_epoch_millis(metadata.created_at))
        .bind(datetime_to_epoch_millis(updated_at))
        .bind(datetime_to_epoch_millis(insert_recency_at))
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
        .bind(creation_memory_mode.unwrap_or("enabled"))
        .execute(self.sqlite_pool()?)
        .await?;
        self.insert_thread_spawn_edge_from_source_if_absent(metadata.id, metadata.source.as_str())
            .await?;
        Ok(())
    }

    /// Apply rollout items incrementally using the underlying database.
    pub async fn apply_rollout_items(
        &self,
        builder: &ThreadMetadataBuilder,
        items: &[RolloutItem],
        new_thread_memory_mode: Option<&str>,
        updated_at_override: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let existing_metadata = self.get_thread(builder.id).await?;
        let mut metadata = existing_metadata
            .clone()
            .unwrap_or_else(|| builder.build(&self.default_provider));
        metadata.rollout_path = builder.rollout_path.clone();
        for item in items {
            apply_rollout_item(&mut metadata, item, &self.default_provider);
        }
        if let Some(existing_metadata) = existing_metadata.as_ref() {
            metadata.prefer_existing_git_info(existing_metadata);
        }
        let updated_at = match updated_at_override {
            Some(updated_at) => Some(updated_at),
            None => file_modified_time_utc(builder.rollout_path.as_path()).await,
        };
        if let Some(updated_at) = updated_at {
            metadata.updated_at = updated_at;
        }
        let upsert_result = if existing_metadata.is_none() {
            self.upsert_thread_with_creation_memory_mode(&metadata, new_thread_memory_mode)
                .await
        } else {
            self.upsert_thread(&metadata).await
        };
        upsert_result?;
        if let Some(memory_mode) = extract_memory_mode(items)
            && let Err(err) = self
                .set_thread_memory_mode(builder.id, memory_mode.as_str())
                .await
        {
            return Err(err);
        }
        Ok(())
    }

    /// Mark a thread as archived using the underlying database.
    pub async fn mark_archived(
        &self,
        thread_id: ThreadId,
        rollout_path: &Path,
        archived_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let Some(mut metadata) = self.get_thread(thread_id).await? else {
            return Ok(());
        };
        metadata.archived_at = Some(archived_at);
        metadata.rollout_path = rollout_path.to_path_buf();
        if let Some(updated_at) = file_modified_time_utc(rollout_path).await {
            metadata.updated_at = updated_at;
        }
        if metadata.id != thread_id {
            warn!(
                "thread id mismatch during archive: expected {thread_id}, got {}",
                metadata.id
            );
        }
        self.upsert_thread(&metadata).await
    }

    /// Mark a thread as unarchived using the underlying database.
    pub async fn mark_unarchived(
        &self,
        thread_id: ThreadId,
        rollout_path: &Path,
    ) -> anyhow::Result<()> {
        let Some(mut metadata) = self.get_thread(thread_id).await? else {
            return Ok(());
        };
        metadata.archived_at = None;
        metadata.rollout_path = rollout_path.to_path_buf();
        if let Some(updated_at) = file_modified_time_utc(rollout_path).await {
            metadata.updated_at = updated_at;
        }
        if metadata.id != thread_id {
            warn!(
                "thread id mismatch during unarchive: expected {thread_id}, got {}",
                metadata.id
            );
        }
        self.upsert_thread(&metadata).await
    }
}

fn extract_memory_mode(items: &[RolloutItem]) -> Option<String> {
    items.iter().rev().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line) => meta_line.meta.memory_mode.clone(),
        RolloutItem::ResponseItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::Compacted(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::EventMsg(_) => None,
    })
}
