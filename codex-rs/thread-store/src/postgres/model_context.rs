use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TruncationPolicy;
use codex_rollout::ModelContextScan;
use codex_rollout::ModelContextScanProgress;
use futures::TryStreamExt;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::PostgresThreadStore;
use super::database_error;
use super::serialization_error;
use crate::LoadThreadHistoryParams;
use crate::StoredModelContext;
use crate::StoredThread;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

const MAX_MODEL_CONTEXT_ITEMS: usize = 10_000;
const MAX_MODEL_CONTEXT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MODEL_CONTEXT_ITEM_TOKENS: usize = 10_000;

fn max_model_context_item_bytes() -> usize {
    TruncationPolicy::Tokens(MAX_MODEL_CONTEXT_ITEM_TOKENS).byte_budget()
}

struct ModelContextBudget {
    thread_id: codex_protocol::ThreadId,
    items: usize,
    bytes: u64,
    max_items: usize,
    max_bytes: u64,
}

impl ModelContextBudget {
    fn new(thread_id: codex_protocol::ThreadId) -> Self {
        Self::with_limits(thread_id, MAX_MODEL_CONTEXT_ITEMS, MAX_MODEL_CONTEXT_BYTES)
    }

    fn with_limits(thread_id: codex_protocol::ThreadId, max_items: usize, max_bytes: u64) -> Self {
        Self {
            thread_id,
            items: 0,
            bytes: 0,
            max_items,
            max_bytes,
        }
    }

    fn account_item(&mut self, item_bytes: i64) -> ThreadStoreResult<()> {
        let item_bytes =
            u64::try_from(item_bytes).map_err(|_| self.limit_error("invalid item size"))?;
        let next_items = self
            .items
            .checked_add(1)
            .ok_or_else(|| self.limit_error("item count overflow"))?;
        let next_bytes = self
            .bytes
            .checked_add(item_bytes)
            .ok_or_else(|| self.limit_error("byte count overflow"))?;
        if next_items > self.max_items || next_bytes > self.max_bytes {
            return Err(self.limit_error("history exceeds the bounded read budget"));
        }
        self.items = next_items;
        self.bytes = next_bytes;
        Ok(())
    }

    fn limit_error(&self, reason: &str) -> ThreadStoreError {
        ThreadStoreError::InvalidRequest {
            message: format!(
                "model context for thread {} cannot be loaded safely: {reason} (limit: {} items or {} bytes)",
                self.thread_id, self.max_items, self.max_bytes
            ),
        }
    }
}

fn bounded_item_from_row(
    row: &PgRow,
    budget: &ModelContextBudget,
) -> ThreadStoreResult<RolloutItem> {
    let item: Option<Value> = row
        .try_get("item")
        .map_err(|error| database_error("load latest model context", error))?;
    let item = item.ok_or_else(|| {
        budget.limit_error(&format!(
            "an individual history item exceeds {MAX_MODEL_CONTEXT_ITEM_TOKENS} estimated tokens"
        ))
    })?;
    serde_json::from_value(item).map_err(serialization_error)
}

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
        "SELECT CASE WHEN octet_length(item::text) <= $2 THEN item END AS item, \
         octet_length(item::text)::bigint AS item_bytes \
         FROM {} WHERE thread_id = $1 AND ordinal = 0",
        store.tables.history
    )))
    .bind(params.thread_id.to_string())
    .bind(i64::try_from(max_model_context_item_bytes()).unwrap_or(i64::MAX))
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error("load latest model context", error))?
    .ok_or_else(|| ThreadStoreError::Internal {
        message: format!("thread {} has no session metadata", params.thread_id),
    })?;
    let head_bytes: i64 = head
        .try_get("item_bytes")
        .map_err(|error| database_error("load latest model context", error))?;
    let mut budget = ModelContextBudget::new(params.thread_id);
    budget.account_item(head_bytes)?;
    let head = bounded_item_from_row(&head, &budget)?;
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
            "SELECT ordinal, CASE WHEN octet_length(item::text) <= $3 THEN item END AS item, \
             octet_length(item::text)::bigint AS item_bytes \
             FROM {} WHERE thread_id = $1 ORDER BY ordinal DESC LIMIT $2",
            store.tables.history
        )))
        .bind(params.thread_id.to_string())
        .bind(i64::try_from(MAX_MODEL_CONTEXT_ITEMS + 1).unwrap_or(i64::MAX))
        .bind(i64::try_from(max_model_context_item_bytes()).unwrap_or(i64::MAX))
        .fetch(transaction.as_mut());
        while let Some(row) = rows
            .try_next()
            .await
            .map_err(|error| database_error("load latest model context", error))?
        {
            let ordinal: i64 = row
                .try_get("ordinal")
                .map_err(|error| database_error("load latest model context", error))?;
            if ordinal != 0 {
                let item_bytes: i64 = row
                    .try_get("item_bytes")
                    .map_err(|error| database_error("load latest model context", error))?;
                budget.account_item(item_bytes)?;
            }
            let item = bounded_item_from_row(&row, &budget)?;
            if matches!(scan.push(item), ModelContextScanProgress::Complete) {
                break;
            }
        }
        drop(rows);
        scan.finish(session_meta)
    } else {
        let mut rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT ordinal, CASE WHEN octet_length(item::text) <= $3 THEN item END AS item, \
             octet_length(item::text)::bigint AS item_bytes \
             FROM {} WHERE thread_id = $1 ORDER BY ordinal ASC LIMIT $2",
            store.tables.history
        )))
        .bind(params.thread_id.to_string())
        .bind(i64::try_from(MAX_MODEL_CONTEXT_ITEMS + 1).unwrap_or(i64::MAX))
        .bind(i64::try_from(max_model_context_item_bytes()).unwrap_or(i64::MAX))
        .fetch(transaction.as_mut());
        let mut items = Vec::new();
        while let Some(row) = rows
            .try_next()
            .await
            .map_err(|error| database_error("load latest model context", error))?
        {
            let ordinal: i64 = row
                .try_get("ordinal")
                .map_err(|error| database_error("load latest model context", error))?;
            if ordinal != 0 {
                let item_bytes: i64 = row
                    .try_get("item_bytes")
                    .map_err(|error| database_error("load latest model context", error))?;
                budget.account_item(item_bytes)?;
            }
            items.push(bounded_item_from_row(&row, &budget)?);
        }
        drop(rows);
        items
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

#[cfg(test)]
#[path = "model_context_tests.rs"]
mod tests;
