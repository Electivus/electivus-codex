use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::SecondsFormat;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Row;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

use crate::AppendBatchCommit;
use crate::AppendThreadItemsBatch;
use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::ChildRegistrationGuard;
use crate::CreateThreadParams;
use crate::DeleteThreadParams;
use crate::ItemPage;
use crate::ListItemsParams;
use crate::ListThreadsParams;
use crate::ListTurnsParams;
use crate::LoadThreadHistoryParams;
use crate::ReadThreadByRolloutPathParams;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::SearchThreadOccurrencesParams;
use crate::SearchThreadsParams;
use crate::StoredModelContext;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadOccurrenceSearchPage;
use crate::ThreadPage;
use crate::ThreadSearchPage;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreFuture;
use crate::ThreadStoreResult;
use crate::TurnPage;
use crate::UpdateThreadMetadataParams;
use crate::thread_metadata_sync::ThreadMetadataSync;
use crate::types::initial_session_meta_item;

mod append;
mod items;
mod lifecycle;
mod list_threads;
mod metadata;
mod migration;
mod model_context;
mod projection;
mod resume;
mod search_thread_occurrences;
mod search_threads;
mod turns;

pub(super) const WRITER_LEASE_DURATION: Duration = Duration::from_secs(30);

/// PostgreSQL-backed implementation of [`ThreadStore`].
///
/// Construction is intentionally separate from runtime backend selection until every Runtime
/// State Store responsibility can select PostgreSQL integrally.
#[derive(Clone)]
pub struct PostgresThreadStore {
    pub(super) pool: sqlx::PgPool,
    state_db: Option<Arc<codex_state::StateRuntime>>,
    pub(super) tables: PostgresThreadTables,
    pub(super) writer_id: String,
    pub(super) live_writers: Arc<Mutex<HashMap<ThreadId, ActiveWriter>>>,
    operation_locks: Arc<Mutex<HashMap<ThreadId, Arc<Mutex<()>>>>>,
}

#[derive(Clone)]
pub(super) struct PostgresThreadTables {
    pub(super) threads: String,
    pub(super) history: String,
    pub(super) append_batches: String,
    pub(super) items: String,
    pub(super) turns: String,
    pub(super) search_content: String,
    pub(super) spawn_edges: String,
}

impl PostgresThreadTables {
    fn new(schema: &str) -> Self {
        let qualified_schema = format!("\"{schema}\"");
        Self {
            threads: format!("{qualified_schema}.threads"),
            history: format!("{qualified_schema}.thread_history"),
            append_batches: format!("{qualified_schema}.thread_append_batches"),
            items: format!("{qualified_schema}.thread_items"),
            turns: format!("{qualified_schema}.thread_turns"),
            search_content: format!("{qualified_schema}.thread_search_content"),
            spawn_edges: format!("{qualified_schema}.thread_spawn_edges"),
        }
    }
}

/// Materializes thread projections during an explicit offline Runtime State Migration.
///
/// This type cannot open a pool or serve runtime traffic. The migration coordinator supplies the
/// transaction after it has validated and fenced the destination namespace.
pub struct PostgresThreadProjectionMaterializer {
    schema: String,
    tables: PostgresThreadTables,
}

impl PostgresThreadProjectionMaterializer {
    pub fn new(config: &codex_state::PostgresNamespaceConfig) -> Self {
        let schema = config.schema().to_string();
        let tables = PostgresThreadTables::new(&schema);
        Self { schema, tables }
    }
}

#[derive(Clone)]
pub(super) struct ActiveWriter {
    pub(super) fencing_token: i64,
    pub(super) expected_stream_version: i64,
    pub(super) history_mode: codex_protocol::protocol::ThreadHistoryMode,
    pub(super) history_projection_start_ordinal: Option<i64>,
    pub(super) metadata_sync: ThreadMetadataSync,
}

impl PostgresThreadStore {
    /// Construct the PostgreSQL Thread Store owned by an integral runtime.
    pub fn from_runtime(state_db: Arc<codex_state::StateRuntime>) -> ThreadStoreResult<Self> {
        let runtime_pool =
            state_db
                .postgres_runtime_pool()
                .ok_or_else(|| ThreadStoreError::Internal {
                    message: "PostgreSQL Thread Store requires a PostgreSQL StateRuntime"
                        .to_string(),
                })?;
        let (pool, schema) = runtime_pool.thread_store_connection();
        Ok(Self::from_connection(pool, schema, Some(state_db)))
    }

    #[cfg(test)]
    pub(crate) fn new(pool: sqlx::PgPool, schema: String) -> Self {
        Self::from_connection(pool, schema, /*state_db*/ None)
    }

    fn from_connection(
        pool: sqlx::PgPool,
        schema: String,
        state_db: Option<Arc<codex_state::StateRuntime>>,
    ) -> Self {
        Self {
            pool,
            tables: PostgresThreadTables::new(&schema),
            state_db,
            writer_id: Uuid::now_v7().to_string(),
            live_writers: Arc::new(Mutex::new(HashMap::new())),
            operation_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn state_db(&self) -> Option<Arc<codex_state::StateRuntime>> {
        self.state_db.clone()
    }

    async fn lock_operation(&self, thread_id: ThreadId) -> OwnedMutexGuard<()> {
        let lock = self
            .operation_locks
            .lock()
            .await
            .entry(thread_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        lock.lock_owned().await
    }

    async fn create(&self, params: CreateThreadParams) -> ThreadStoreResult<()> {
        let created_at = metadata::postgres_timestamp(Utc::now());
        let session_meta = initial_session_meta_item(
            &params,
            created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            env!("CARGO_PKG_VERSION").to_string(),
        );
        let projection = StoredThread {
            thread_id: params.thread_id,
            extra_config: params.extra_config.clone(),
            rollout_path: None,
            forked_from_id: params.forked_from_id,
            parent_thread_id: params.parent_thread_id,
            preview: String::new(),
            name: None,
            model_provider: params.metadata.model_provider.clone(),
            model: None,
            reasoning_effort: None,
            created_at,
            updated_at: created_at,
            recency_at: created_at,
            archived_at: None,
            is_pinned: false,
            cwd: params.metadata.cwd.clone().unwrap_or_default(),
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            source: params.source.clone(),
            history_mode: params.history_mode,
            thread_source: params.thread_source.clone(),
            agent_nickname: params.source.get_nickname(),
            agent_role: params.source.get_agent_role(),
            agent_path: params.source.get_agent_path().map(Into::into),
            git_info: None,
            repository_identity: None,
            approval_mode: AskForApproval::OnRequest,
            permission_profile: PermissionProfile::read_only(),
            token_usage: None,
            first_user_message: None,
            history: None,
        };
        let metadata_sync = ThreadMetadataSync::from_stored_thread(&projection);
        let projection = serde_json::to_value(projection).map_err(serialization_error)?;
        let session_meta = serde_json::to_value(session_meta).map_err(serialization_error)?;
        let lease_millis = i64::try_from(WRITER_LEASE_DURATION.as_millis()).map_err(|_| {
            ThreadStoreError::Internal {
                message: "thread writer lease duration is out of range".to_string(),
            }
        })?;
        let history_projection_start_ordinal = params
            .subagent_history_start_ordinal
            .map(i64::try_from)
            .transpose()
            .map_err(|_| ThreadStoreError::InvalidRequest {
                message: "subagent history start ordinal is too large".to_string(),
            })?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| database_error("create", error))?;
        if let Some(parent_thread_id) = params.parent_thread_id {
            let parent = sqlx::query_scalar::<_, String>(AssertSqlSafe(format!(
                "SELECT thread_id FROM {} WHERE thread_id = $1 FOR KEY SHARE",
                self.tables.threads
            )))
            .bind(parent_thread_id.to_string())
            .fetch_optional(transaction.as_mut())
            .await
            .map_err(|error| database_error("create", error))?;
            if parent.is_none() {
                return Err(ThreadStoreError::ThreadNotFound {
                    thread_id: parent_thread_id,
                });
            }
        }
        let insert_thread = sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (thread_id, projection, stream_version, history_projection_version, fencing_token, writer_id, writer_lease_expires_at, created_at, updated_at, recency_at, history_projection_start_ordinal) \
             VALUES ($1, $2, 1, 1, 1, $3, CURRENT_TIMESTAMP + $4 * INTERVAL '1 millisecond', $5, $5, $5, $6)",
            self.tables.threads
        )))
        .bind(params.thread_id.to_string())
        .bind(projection)
        .bind(&self.writer_id)
        .bind(lease_millis)
        .bind(created_at)
        .bind(history_projection_start_ordinal)
        .execute(transaction.as_mut())
        .await;
        if let Err(error) = insert_thread {
            if error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref()
                == Some("23505")
            {
                return Err(ThreadStoreError::Conflict {
                    message: format!("thread {} already exists", params.thread_id),
                });
            }
            return Err(database_error("create", error));
        }
        if let Some(parent_thread_id) = params.parent_thread_id {
            sqlx::query(AssertSqlSafe(format!(
                "INSERT INTO {} (parent_thread_id, child_thread_id, status) \
                 VALUES ($1, $2, 'open')",
                self.tables.spawn_edges
            )))
            .bind(parent_thread_id.to_string())
            .bind(params.thread_id.to_string())
            .execute(transaction.as_mut())
            .await
            .map_err(|error| database_error("create", error))?;
        }
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (thread_id, ordinal, item, recorded_at) VALUES ($1, 0, $2, $3)",
            self.tables.history
        )))
        .bind(params.thread_id.to_string())
        .bind(session_meta)
        .bind(created_at)
        .execute(transaction.as_mut())
        .await
        .map_err(|error| database_error("create", error))?;
        transaction
            .commit()
            .await
            .map_err(|error| database_error("create", error))?;
        self.live_writers.lock().await.insert(
            params.thread_id,
            ActiveWriter {
                fencing_token: 1,
                expected_stream_version: 1,
                history_mode: params.history_mode,
                history_projection_start_ordinal,
                metadata_sync,
            },
        );
        Ok(())
    }

    async fn read(&self, params: ReadThreadParams) -> ThreadStoreResult<StoredThread> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT projection FROM {} WHERE thread_id = $1",
            self.tables.threads
        )))
        .bind(params.thread_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| database_error("read", error))?
        .ok_or(ThreadStoreError::ThreadNotFound {
            thread_id: params.thread_id,
        })?;
        let projection: Value = row
            .try_get("projection")
            .map_err(|error| database_error("read", error))?;
        let mut thread: StoredThread =
            serde_json::from_value(projection).map_err(serialization_error)?;
        if !params.include_archived && thread.archived_at.is_some() {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!("thread {} is archived", params.thread_id),
            });
        }
        if params.include_history {
            let rows = sqlx::query(AssertSqlSafe(format!(
                "SELECT item FROM {} WHERE thread_id = $1 ORDER BY ordinal ASC",
                self.tables.history
            )))
            .bind(params.thread_id.to_string())
            .fetch_all(&self.pool)
            .await
            .map_err(|error| database_error("load history", error))?;
            let items = rows
                .into_iter()
                .map(|row| {
                    let value: Value = row
                        .try_get("item")
                        .map_err(|error| database_error("load history", error))?;
                    serde_json::from_value(value).map_err(serialization_error)
                })
                .collect::<ThreadStoreResult<Vec<_>>>()?;
            thread.history = Some(StoredThreadHistory {
                thread_id: params.thread_id,
                items,
            });
        }
        Ok(thread)
    }
}

impl codex_state::RuntimeStateThreadProjectionMaterializer
    for PostgresThreadProjectionMaterializer
{
    type Error = ThreadStoreError;

    fn destination_schema(&self) -> &str {
        &self.schema
    }

    async fn materialize(
        &self,
        connection: &mut sqlx::PgConnection,
        snapshot: &codex_state::RuntimeStateThreadSnapshot,
    ) -> Result<(), Self::Error> {
        for thread in snapshot.threads() {
            let row = sqlx::query(AssertSqlSafe(format!(
                "SELECT stream_version, history_projection_start_ordinal FROM {} \
                 WHERE thread_id = $1",
                self.tables.threads
            )))
            .bind(thread.metadata().id.to_string())
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| database_error("materialize migrated thread projections", error))?;
            projection::rebuild_history_projections_from_lines(
                &self.tables,
                connection,
                thread.metadata().id,
                row.try_get("stream_version").map_err(|error| {
                    database_error("materialize migrated thread projections", error)
                })?,
                row.try_get("history_projection_start_ordinal")
                    .map_err(|error| {
                        database_error("materialize migrated thread projections", error)
                    })?,
                thread.canonical_history().lines(),
            )
            .await?;
            migration::validate_migrated_thread_projections(&self.tables, connection, thread)
                .await?;
        }
        Ok(())
    }
}

impl ThreadStore for PostgresThreadStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let _operation_guard = self.lock_operation(params.thread_id).await;
            self.create(params).await
        })
    }

    fn resume_thread(&self, params: ResumeThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let _operation_guard = self.lock_operation(params.thread_id).await;
            resume::resume_thread(self, params).await
        })
    }

    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let _operation_guard = self.lock_operation(params.thread_id).await;
            append::append_items(self, params).await
        })
    }

    fn append_batch(
        &self,
        batch: AppendThreadItemsBatch,
    ) -> ThreadStoreFuture<'_, AppendBatchCommit> {
        Box::pin(async move {
            let _operation_guard = self.lock_operation(batch.thread_id).await;
            append::append_batch(self, batch).await
        })
    }

    fn persist_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let _operation_guard = self.lock_operation(thread_id).await;
            lifecycle::renew_writer(self, thread_id).await
        })
    }
    fn flush_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let _operation_guard = self.lock_operation(thread_id).await;
            lifecycle::renew_writer(self, thread_id).await
        })
    }
    fn shutdown_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let _operation_guard = self.lock_operation(thread_id).await;
            lifecycle::release_writer(self, thread_id).await
        })
    }
    fn discard_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let _operation_guard = self.lock_operation(thread_id).await;
            lifecycle::release_writer(self, thread_id).await
        })
    }

    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory> {
        Box::pin(async move {
            self.read(ReadThreadParams {
                thread_id: params.thread_id,
                include_archived: params.include_archived,
                include_history: true,
            })
            .await?
            .history
            .ok_or_else(|| ThreadStoreError::Internal {
                message: format!("thread {} history was not loaded", params.thread_id),
            })
        })
    }

    fn load_latest_model_context(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredModelContext> {
        Box::pin(model_context::load_latest_model_context(self, params))
    }

    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(self.read(params))
    }

    fn validate_child_registration(
        &self,
        thread_id: ThreadId,
    ) -> ThreadStoreFuture<'_, ChildRegistrationGuard> {
        Box::pin(async move {
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(|error| database_error("validate child registration", error))?;
            let persisted_thread_id = sqlx::query_scalar::<_, String>(AssertSqlSafe(format!(
                "SELECT thread_id FROM {} WHERE thread_id = $1 FOR KEY SHARE",
                self.tables.threads
            )))
            .bind(thread_id.to_string())
            .fetch_optional(transaction.as_mut())
            .await
            .map_err(|error| database_error("validate child registration", error))?;
            if persisted_thread_id.is_none() {
                return Err(ThreadStoreError::ThreadNotFound { thread_id });
            }
            Ok(ChildRegistrationGuard::holding(transaction))
        })
    }

    fn read_thread_by_rollout_path(
        &self,
        _params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        unsupported("read_thread_by_rollout_path")
    }
    fn supports_rollout_path_lookup(&self) -> bool {
        false
    }
    fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage> {
        Box::pin(list_threads::list_threads(self, params))
    }
    fn supports_paginated_history_lists(&self) -> bool {
        true
    }
    fn supports_paginated_rollback(&self) -> bool {
        true
    }
    fn search_threads(
        &self,
        params: SearchThreadsParams,
    ) -> ThreadStoreFuture<'_, ThreadSearchPage> {
        Box::pin(search_threads::search_threads(self, params))
    }
    fn search_thread_occurrences(
        &self,
        params: SearchThreadOccurrencesParams,
    ) -> ThreadStoreFuture<'_, ThreadOccurrenceSearchPage> {
        Box::pin(search_thread_occurrences::search_thread_occurrences(
            self, params,
        ))
    }
    fn list_turns(&self, params: ListTurnsParams) -> ThreadStoreFuture<'_, TurnPage> {
        Box::pin(turns::list_turns(self, params))
    }
    fn list_items(&self, params: ListItemsParams) -> ThreadStoreFuture<'_, ItemPage> {
        Box::pin(items::list_items(self, params))
    }
    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move {
            let _operation_guard = self.lock_operation(params.thread_id).await;
            metadata::update_thread_metadata(self, params).await
        })
    }
    fn archive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let _operation_guard = self.lock_operation(params.thread_id).await;
            lifecycle::archive_thread(self, params).await
        })
    }
    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move {
            let _operation_guard = self.lock_operation(params.thread_id).await;
            lifecycle::unarchive_thread(self, params).await
        })
    }
    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let _operation_guard = self.lock_operation(params.thread_id).await;
            lifecycle::delete_thread(self, params).await
        })
    }
}

fn unsupported<T>(operation: &'static str) -> ThreadStoreFuture<'static, T> {
    Box::pin(async move { Err(ThreadStoreError::Unsupported { operation }) })
}

pub(super) fn serialization_error(error: serde_json::Error) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("thread store could not encode durable thread data: {error}"),
    }
}

pub(super) fn database_error(operation: &'static str, _error: sqlx::Error) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!(
            "thread store could not complete `{operation}`; verify persistence health, then retry"
        ),
    }
}

pub(super) fn writer_conflict(thread_id: ThreadId) -> ThreadStoreError {
    ThreadStoreError::Conflict {
        message: format!("thread {thread_id} writer no longer owns the expected stream version"),
    }
}
