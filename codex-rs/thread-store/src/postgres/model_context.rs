use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::ModelContextScan;
use codex_rollout::ModelContextScanProgress;
use codex_rollout::RolloutItem;
use futures::TryStreamExt;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Row;

use super::PostgresThreadStore;
use super::database_error;
use super::serialization_error;
use crate::LoadThreadHistoryParams;
use crate::StoredModelContext;
use crate::StoredThread;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn load_latest_model_context(
    store: &PostgresThreadStore,
    params: LoadThreadHistoryParams,
) -> ThreadStoreResult<StoredModelContext> {
    let mut transaction = store
        .pool
        .begin()
        .await
        .map_err(|error| database_error("load latest model context", error))?;
    let projection = sqlx::query(AssertSqlSafe(format!(
        "SELECT projection FROM {} WHERE thread_id = $1 FOR SHARE",
        store.tables.threads
    )))
    .bind(params.thread_id.to_string())
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error("load latest model context", error))?
    .ok_or(ThreadStoreError::ThreadNotFound {
        thread_id: params.thread_id,
    })?;
    let projection: Value = projection
        .try_get("projection")
        .map_err(|error| database_error("load latest model context", error))?;
    let thread: StoredThread = serde_json::from_value(projection).map_err(serialization_error)?;
    if !params.include_archived && thread.archived_at.is_some() {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("thread {} is archived", params.thread_id),
        });
    }

    let head = sqlx::query(AssertSqlSafe(format!(
        "SELECT item FROM {} WHERE thread_id = $1 AND ordinal = 0",
        store.tables.history
    )))
    .bind(params.thread_id.to_string())
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error("load latest model context", error))?
    .ok_or_else(|| ThreadStoreError::Internal {
        message: format!("thread {} has no session metadata", params.thread_id),
    })?;
    let head: Value = head
        .try_get("item")
        .map_err(|error| database_error("load latest model context", error))?;
    let head: RolloutItem = serde_json::from_value(head).map_err(serialization_error)?;
    let RolloutItem::SessionMeta(session_meta) = head else {
        return Err(ThreadStoreError::Internal {
            message: format!("thread {} has invalid session metadata", params.thread_id),
        });
    };
    if session_meta.meta.id != params.thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "thread history belongs to {}, not {}",
                session_meta.meta.id, params.thread_id
            ),
        });
    }

    let items = if matches!(session_meta.meta.history_mode, ThreadHistoryMode::Paginated) {
        let mut scan = ModelContextScan::default();
        let mut rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT item FROM {} WHERE thread_id = $1 ORDER BY ordinal DESC",
            store.tables.history
        )))
        .bind(params.thread_id.to_string())
        .fetch(transaction.as_mut());
        while let Some(row) = rows
            .try_next()
            .await
            .map_err(|error| database_error("load latest model context", error))?
        {
            let item: Value = row
                .try_get("item")
                .map_err(|error| database_error("load latest model context", error))?;
            let item = serde_json::from_value(item).map_err(serialization_error)?;
            if matches!(scan.push(item), ModelContextScanProgress::Complete) {
                break;
            }
        }
        drop(rows);
        scan.finish(session_meta)
    } else {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT item FROM {} WHERE thread_id = $1 ORDER BY ordinal ASC",
            store.tables.history
        )))
        .bind(params.thread_id.to_string())
        .fetch_all(transaction.as_mut())
        .await
        .map_err(|error| database_error("load latest model context", error))?;
        rows.into_iter()
            .map(|row| {
                let item: Value = row
                    .try_get("item")
                    .map_err(|error| database_error("load latest model context", error))?;
                serde_json::from_value(item).map_err(serialization_error)
            })
            .collect::<ThreadStoreResult<Vec<_>>>()?
    };

    transaction
        .commit()
        .await
        .map_err(|error| database_error("load latest model context", error))?;
    Ok(StoredModelContext {
        thread_id: params.thread_id,
        items,
    })
}
