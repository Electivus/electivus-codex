use chrono::DateTime;
use chrono::Utc;
use codex_git_utils::GitSha;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::GitInfo;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadMemoryMode;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Row;

use super::PostgresThreadStore;
use super::database_error;
use super::serialization_error;
use super::writer_conflict;
use crate::GitInfoPatch;
use crate::StoredThread;
use crate::ThreadMetadataPatch;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::UpdateThreadMetadataParams;
use crate::thread_metadata_sync::ThreadMetadataSync;

pub(super) async fn update_thread_metadata(
    store: &PostgresThreadStore,
    params: UpdateThreadMetadataParams,
) -> ThreadStoreResult<StoredThread> {
    let mut transaction = store
        .pool
        .begin()
        .await
        .map_err(|error| database_error("update thread metadata", error))?;
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT projection, stream_version, fencing_token, writer_id, \
         writer_lease_expires_at > CURRENT_TIMESTAMP AS lease_active, \
         CURRENT_TIMESTAMP AS database_now, created_at, updated_at, recency_at, archived_at, \
         is_pinned \
         FROM {} WHERE thread_id = $1 FOR UPDATE",
        store.tables.threads
    )))
    .bind(params.thread_id.to_string())
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error("update thread metadata", error))?
    .ok_or(ThreadStoreError::ThreadNotFound {
        thread_id: params.thread_id,
    })?;
    let archived_at: Option<DateTime<Utc>> = row
        .try_get("archived_at")
        .map_err(|error| database_error("update thread metadata", error))?;
    if !params.include_archived && archived_at.is_some() {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("thread {} is archived", params.thread_id),
        });
    }
    let stream_version: i64 = row
        .try_get("stream_version")
        .map_err(|error| database_error("update thread metadata", error))?;
    let projection_json: Value = row
        .try_get("projection")
        .map_err(|error| database_error("update thread metadata", error))?;
    let mut history = None;
    let mut projection = match serde_json::from_value(projection_json) {
        Ok(projection) => projection,
        Err(_) => {
            let items = load_history(store, &mut transaction, params.thread_id).await?;
            let projection = rebuild_projection(params.thread_id, &row, items.as_slice())?;
            history = Some(items);
            projection
        }
    };
    let writes_canonical_metadata =
        params.patch.git_info.is_some() || params.patch.memory_mode.is_some();
    let mut next_stream_version = stream_version;
    if writes_canonical_metadata {
        let fencing_token: i64 = row
            .try_get("fencing_token")
            .map_err(|error| database_error("update thread metadata", error))?;
        let writer_id: String = row
            .try_get("writer_id")
            .map_err(|error| database_error("update thread metadata", error))?;
        let lease_active: bool = row
            .try_get("lease_active")
            .map_err(|error| database_error("update thread metadata", error))?;
        if lease_active && writer_id != store.writer_id {
            return Err(writer_conflict(params.thread_id));
        }
        if lease_active {
            let owns_expected_stream = store
                .live_writers
                .lock()
                .await
                .get(&params.thread_id)
                .is_some_and(|writer| {
                    writer.fencing_token == fencing_token
                        && writer.expected_stream_version == stream_version
                });
            if !owns_expected_stream {
                return Err(writer_conflict(params.thread_id));
            }
        }
        let items = match history.take() {
            Some(items) => items,
            None => load_history(store, &mut transaction, params.thread_id).await?,
        };
        let mut session_meta = effective_session_meta(params.thread_id, items.as_slice())?;
        projection.git_info = normalized_git_info(session_meta.git.clone());
        apply_canonical_metadata_patch(&mut session_meta, &params.patch);
        if params.patch.git_info.is_some() {
            projection.git_info = normalized_git_info(session_meta.git.clone());
        } else {
            session_meta.git = None;
        }
        let item = serde_json::to_value(RolloutItem::SessionMeta(session_meta))
            .map_err(serialization_error)?;
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (thread_id, ordinal, item) VALUES ($1, $2, $3)",
            store.tables.history
        )))
        .bind(params.thread_id.to_string())
        .bind(stream_version)
        .bind(item)
        .execute(transaction.as_mut())
        .await
        .map_err(|error| database_error("update thread metadata", error))?;
        next_stream_version =
            stream_version
                .checked_add(1)
                .ok_or_else(|| ThreadStoreError::Internal {
                    message: "thread history is too large to persist".to_string(),
                })?;
    }
    apply_metadata_patch(&mut projection, &params.patch);
    if writes_canonical_metadata && params.patch.updated_at.is_none() {
        let database_now = row
            .try_get("database_now")
            .map_err(|error| database_error("update thread metadata", error))?;
        projection.updated_at = projection.updated_at.max(postgres_timestamp(database_now));
    }
    let projection_json = serde_json::to_value(&projection).map_err(serialization_error)?;
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET projection = $1, \
         history_projection_version = CASE \
             WHEN history_projection_version = stream_version THEN $2 \
             ELSE history_projection_version \
         END, \
         stream_version = $2, created_at = $3, updated_at = $4, recency_at = $5, is_pinned = $6, \
         repository_identity = $7 WHERE thread_id = $8",
        store.tables.threads
    )))
    .bind(projection_json)
    .bind(next_stream_version)
    .bind(projection.created_at)
    .bind(projection.updated_at)
    .bind(projection.recency_at)
    .bind(projection.is_pinned)
    .bind(projection.repository_identity.as_deref())
    .bind(params.thread_id.to_string())
    .execute(transaction.as_mut())
    .await
    .map_err(|error| database_error("update thread metadata", error))?;
    transaction
        .commit()
        .await
        .map_err(|error| database_error("update thread metadata", error))?;
    if let Some(writer) = store.live_writers.lock().await.get_mut(&params.thread_id)
        && writer.expected_stream_version == stream_version
    {
        writer.expected_stream_version = next_stream_version;
        writer.metadata_sync = ThreadMetadataSync::from_stored_thread(&projection);
    }
    Ok(projection)
}

async fn load_history(
    store: &PostgresThreadStore,
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    thread_id: ThreadId,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    let rows = sqlx::query(AssertSqlSafe(format!(
        "SELECT item FROM {} WHERE thread_id = $1 ORDER BY ordinal ASC",
        store.tables.history
    )))
    .bind(thread_id.to_string())
    .fetch_all(transaction.as_mut())
    .await
    .map_err(|error| database_error("repair thread metadata", error))?;
    rows.into_iter()
        .map(|row| {
            let value: Value = row
                .try_get("item")
                .map_err(|error| database_error("repair thread metadata", error))?;
            serde_json::from_value(value).map_err(serialization_error)
        })
        .collect()
}

fn rebuild_projection(
    thread_id: ThreadId,
    row: &sqlx::postgres::PgRow,
    history: &[RolloutItem],
) -> ThreadStoreResult<StoredThread> {
    let session_meta = effective_session_meta(thread_id, history)?;
    let created_at = row
        .try_get("created_at")
        .map_err(|error| database_error("repair thread metadata", error))?;
    let updated_at = row
        .try_get("updated_at")
        .map_err(|error| database_error("repair thread metadata", error))?;
    let recency_at = row
        .try_get("recency_at")
        .map_err(|error| database_error("repair thread metadata", error))?;
    let archived_at = row
        .try_get("archived_at")
        .map_err(|error| database_error("repair thread metadata", error))?;
    let is_pinned = row
        .try_get("is_pinned")
        .map_err(|error| database_error("repair thread metadata", error))?;
    let mut projection = StoredThread {
        thread_id,
        extra_config: None,
        rollout_path: None,
        forked_from_id: session_meta.meta.forked_from_id,
        parent_thread_id: session_meta.meta.parent_thread_id,
        preview: String::new(),
        name: None,
        model_provider: session_meta.meta.model_provider.clone().unwrap_or_default(),
        model: None,
        reasoning_effort: None,
        created_at,
        updated_at,
        recency_at,
        archived_at,
        is_pinned,
        cwd: session_meta.meta.cwd.clone(),
        cli_version: session_meta.meta.cli_version.clone(),
        source: session_meta.meta.source.clone(),
        history_mode: session_meta.meta.history_mode,
        thread_source: session_meta.meta.thread_source.clone(),
        agent_nickname: session_meta.meta.agent_nickname.clone(),
        agent_role: session_meta.meta.agent_role.clone(),
        agent_path: session_meta.meta.agent_path.clone(),
        repository_identity: session_meta
            .git
            .as_ref()
            .and_then(|git| git.repository_url.as_deref())
            .and_then(codex_git_utils::canonicalize_git_remote_url),
        git_info: normalized_git_info(session_meta.git),
        approval_mode: AskForApproval::OnRequest,
        permission_profile: PermissionProfile::read_only(),
        token_usage: None,
        first_user_message: None,
        history: None,
    };
    let mut sync = ThreadMetadataSync::from_stored_thread(&projection);
    sync.record_resume_history(history);
    if let Some(update) = sync.take_pending_update() {
        apply_metadata_patch(&mut projection, &update.patch);
    }
    projection.updated_at = updated_at;
    projection.recency_at = recency_at;
    projection.archived_at = archived_at;
    Ok(projection)
}

fn effective_session_meta(
    thread_id: ThreadId,
    history: &[RolloutItem],
) -> ThreadStoreResult<SessionMetaLine> {
    let mut latest = None;
    let mut git = None;
    let mut memory_mode = None;
    for item in history {
        if let RolloutItem::SessionMeta(session_meta) = item
            && session_meta.meta.id == thread_id
        {
            latest = Some(session_meta.clone());
            if session_meta.git.is_some() {
                git = session_meta.git.clone();
            }
            if session_meta.meta.memory_mode.is_some() {
                memory_mode = session_meta.meta.memory_mode.clone();
            }
        }
    }
    let mut latest = latest.ok_or_else(|| ThreadStoreError::Internal {
        message: format!("canonical history for thread {thread_id} has no session metadata"),
    })?;
    latest.git = git;
    latest.meta.memory_mode = memory_mode;
    Ok(latest)
}

fn apply_canonical_metadata_patch(meta: &mut SessionMetaLine, patch: &ThreadMetadataPatch) {
    if let Some(git_info) = patch.git_info.as_ref() {
        meta.git = apply_git_info_patch(meta.git.take(), git_info);
    }
    if let Some(memory_mode) = patch.memory_mode {
        meta.meta.memory_mode = Some(
            match memory_mode {
                ThreadMemoryMode::Enabled => "enabled",
                ThreadMemoryMode::Disabled => "disabled",
            }
            .to_string(),
        );
    }
}

pub(super) fn apply_metadata_patch(thread: &mut StoredThread, patch: &ThreadMetadataPatch) {
    if let Some(name) = patch.name.clone() {
        thread.name = name;
    } else if let Some(title) = patch.title.as_deref()
        && thread.history_mode == codex_protocol::protocol::ThreadHistoryMode::Legacy
    {
        let title = title.trim();
        let first_user_message = patch
            .first_user_message
            .as_deref()
            .or(thread.first_user_message.as_deref());
        let matches_first_message = first_user_message.map(str::trim) == Some(title);
        thread.name = (!title.is_empty() && !matches_first_message).then(|| title.to_string());
    }
    if let Some(preview) = patch.preview.clone() {
        thread.preview = preview;
    }
    if let Some(model_provider) = patch.model_provider.clone() {
        thread.model_provider = model_provider;
    }
    if let Some(model) = patch.model.clone() {
        thread.model = Some(model);
    }
    if let Some(reasoning_effort) = patch.reasoning_effort.clone() {
        thread.reasoning_effort = reasoning_effort;
    }
    if let Some(created_at) = patch.created_at {
        thread.created_at = postgres_timestamp(created_at);
    }
    if let Some(updated_at) = patch.updated_at {
        thread.updated_at = postgres_timestamp(updated_at);
    }
    if let Some(recency_at) = patch.advance_recency_at {
        thread.recency_at = thread.recency_at.max(postgres_timestamp(recency_at));
    }
    if let Some(source) = patch.source.clone() {
        thread.source = source;
    }
    if let Some(thread_source) = patch.thread_source.clone() {
        thread.thread_source = thread_source;
    }
    if let Some(agent_nickname) = patch.agent_nickname.clone() {
        thread.agent_nickname = agent_nickname;
    }
    if let Some(agent_role) = patch.agent_role.clone() {
        thread.agent_role = agent_role;
    }
    if let Some(agent_path) = patch.agent_path.clone() {
        thread.agent_path = agent_path;
    }
    if let Some(cwd) = patch.cwd.clone() {
        thread.cwd = cwd;
    }
    if let Some(cli_version) = patch.cli_version.clone() {
        thread.cli_version = cli_version;
    }
    if let Some(approval_mode) = patch.approval_mode {
        thread.approval_mode = approval_mode;
    }
    if let Some(permission_profile) = patch.permission_profile.clone() {
        thread.permission_profile = permission_profile;
    }
    if let Some(token_usage) = patch.token_usage.clone() {
        thread.token_usage = Some(token_usage);
    }
    if let Some(first_user_message) = patch.first_user_message.clone() {
        thread.first_user_message = Some(first_user_message);
    }
    if let Some(is_pinned) = patch.is_pinned {
        thread.is_pinned = is_pinned;
    }
    if let Some(git_info) = patch.git_info.as_ref() {
        thread.git_info =
            normalized_git_info(apply_git_info_patch(thread.git_info.take(), git_info));
        if git_info.origin_url.is_some() {
            thread.repository_identity = thread
                .git_info
                .as_ref()
                .and_then(|info| info.repository_url.as_deref())
                .and_then(codex_git_utils::canonicalize_git_remote_url);
        }
    }
}

fn apply_git_info_patch(existing: Option<GitInfo>, patch: &GitInfoPatch) -> Option<GitInfo> {
    let (existing_sha, existing_branch, existing_origin_url) = match existing {
        Some(info) => (
            info.commit_hash.map(|sha| sha.0),
            info.branch,
            info.repository_url,
        ),
        None => (None, None, None),
    };
    let sha = patch.sha.clone().unwrap_or(existing_sha);
    let branch = patch.branch.clone().unwrap_or(existing_branch);
    let repository_url = patch.origin_url.clone().unwrap_or(existing_origin_url);
    Some(GitInfo {
        commit_hash: sha.as_deref().map(GitSha::new),
        branch,
        repository_url,
    })
}

fn normalized_git_info(git_info: Option<GitInfo>) -> Option<GitInfo> {
    git_info.filter(|info| {
        info.commit_hash.is_some() || info.branch.is_some() || info.repository_url.is_some()
    })
}

pub(super) fn postgres_timestamp(timestamp: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(timestamp.timestamp_millis()).unwrap_or(timestamp)
}
