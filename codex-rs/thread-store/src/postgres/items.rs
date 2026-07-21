use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Postgres;
use sqlx::QueryBuilder;
use sqlx::Row;

use super::PostgresThreadStore;
use super::database_error;
use super::serialization_error;
use crate::ItemPage;
use crate::ListItemsParams;
use crate::SortDirection;
use crate::StoredThread;
use crate::StoredThreadItem;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryCursor {
    thread_id: ThreadId,
    scope: CursorScope,
    rollout_ordinal: i64,
    include_anchor: bool,
}

#[derive(Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum CursorScope {
    Items,
}

struct StoredThreadItemRow {
    item: StoredThreadItem,
    rollout_ordinal: i64,
}

pub(super) async fn list_items(
    store: &PostgresThreadStore,
    params: ListItemsParams,
) -> ThreadStoreResult<ItemPage> {
    validate_thread(store, &params).await?;
    let cursor = parse_cursor(params.cursor.as_deref(), params.thread_id)?;
    let limit = page_limit(params.page_size)?;
    let mut query = QueryBuilder::<Postgres>::new(format!(
        "SELECT turn_id, item_id, rollout_ordinal, created_at_ms, item FROM {} WHERE thread_id = ",
        store.tables.items
    ));
    query.push_bind(params.thread_id.to_string());
    if let Some(turn_id) = params.turn_id.as_deref() {
        query.push(" AND turn_id = ").push_bind(turn_id);
    }
    push_pagination_clause(&mut query, params.sort_direction, cursor.as_ref(), limit);
    let rows = query
        .build()
        .fetch_all(&store.pool)
        .await
        .map_err(|error| database_error("list thread items", error))?;
    let mut item_rows = rows
        .into_iter()
        .map(stored_thread_item_row)
        .collect::<ThreadStoreResult<Vec<_>>>()?;
    let has_more = item_rows.len() > params.page_size;
    item_rows.truncate(params.page_size);
    let backwards_cursor = item_rows
        .first()
        .map(|row| serialize_cursor(params.thread_id, row.rollout_ordinal, true))
        .transpose()?;
    let next_cursor = if has_more {
        item_rows
            .last()
            .map(|row| serialize_cursor(params.thread_id, row.rollout_ordinal, false))
            .transpose()?
    } else {
        None
    };
    Ok(ItemPage {
        items: item_rows.into_iter().map(|row| row.item).collect(),
        next_cursor,
        backwards_cursor,
    })
}

async fn validate_thread(
    store: &PostgresThreadStore,
    params: &ListItemsParams,
) -> ThreadStoreResult<()> {
    let projection = sqlx::query(AssertSqlSafe(format!(
        "SELECT projection FROM {} WHERE thread_id = $1",
        store.tables.threads
    )))
    .bind(params.thread_id.to_string())
    .fetch_optional(&store.pool)
    .await
    .map_err(|error| database_error("list thread items", error))?
    .ok_or(ThreadStoreError::Unsupported {
        operation: "list_items",
    })?
    .try_get::<Value, _>("projection")
    .map_err(|error| database_error("list thread items", error))?;
    let thread: StoredThread = serde_json::from_value(projection).map_err(serialization_error)?;
    if thread.archived_at.is_some() && !params.include_archived {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("thread {} is archived", params.thread_id),
        });
    }
    match thread.history_mode {
        ThreadHistoryMode::Legacy => Err(ThreadStoreError::Unsupported {
            operation: "list_items",
        }),
        ThreadHistoryMode::Paginated => Ok(()),
    }
}

fn page_limit(page_size: usize) -> ThreadStoreResult<i64> {
    if page_size == 0 {
        return Err(ThreadStoreError::InvalidRequest {
            message: "page size must be positive".to_string(),
        });
    }
    let limit = page_size
        .checked_add(1)
        .ok_or_else(|| ThreadStoreError::InvalidRequest {
            message: "page size is too large".to_string(),
        })?;
    i64::try_from(limit).map_err(|_| ThreadStoreError::InvalidRequest {
        message: "page size is too large".to_string(),
    })
}

fn parse_cursor(
    cursor: Option<&str>,
    thread_id: ThreadId,
) -> ThreadStoreResult<Option<HistoryCursor>> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let cursor_value: HistoryCursor =
        serde_json::from_str(cursor).map_err(|_| invalid_cursor(cursor))?;
    if cursor_value.thread_id != thread_id || cursor_value.scope != CursorScope::Items {
        return Err(invalid_cursor(cursor));
    }
    Ok(Some(cursor_value))
}

fn push_pagination_clause(
    query: &mut QueryBuilder<Postgres>,
    direction: SortDirection,
    cursor: Option<&HistoryCursor>,
    limit: i64,
) {
    if let Some(cursor) = cursor {
        let comparator = match (direction, cursor.include_anchor) {
            (SortDirection::Asc, true) => ">=",
            (SortDirection::Asc, false) => ">",
            (SortDirection::Desc, true) => "<=",
            (SortDirection::Desc, false) => "<",
        };
        query
            .push(" AND rollout_ordinal ")
            .push(comparator)
            .push(" ")
            .push_bind(cursor.rollout_ordinal);
    }
    let order = match direction {
        SortDirection::Asc => "ASC",
        SortDirection::Desc => "DESC",
    };
    query
        .push(" ORDER BY rollout_ordinal ")
        .push(order)
        .push(" LIMIT ")
        .push_bind(limit);
}

fn serialize_cursor(
    thread_id: ThreadId,
    rollout_ordinal: i64,
    include_anchor: bool,
) -> ThreadStoreResult<String> {
    serde_json::to_string(&HistoryCursor {
        thread_id,
        scope: CursorScope::Items,
        rollout_ordinal,
        include_anchor,
    })
    .map_err(serialization_error)
}

fn invalid_cursor(cursor: &str) -> ThreadStoreError {
    ThreadStoreError::InvalidRequest {
        message: format!("invalid cursor: {cursor}"),
    }
}

fn stored_thread_item_row(row: sqlx::postgres::PgRow) -> ThreadStoreResult<StoredThreadItemRow> {
    let rollout_ordinal = row
        .try_get::<i64, _>("rollout_ordinal")
        .map_err(|error| database_error("list thread items", error))?;
    let item = row
        .try_get::<Value, _>("item")
        .map_err(|error| database_error("list thread items", error))?;
    Ok(StoredThreadItemRow {
        item: StoredThreadItem {
            turn_id: row
                .try_get("turn_id")
                .map_err(|error| database_error("list thread items", error))?,
            item_id: row
                .try_get("item_id")
                .map_err(|error| database_error("list thread items", error))?,
            created_at_ms: row
                .try_get("created_at_ms")
                .map_err(|error| database_error("list thread items", error))?,
            item_json: serde_json::to_vec(&item).map_err(serialization_error)?,
        },
        rollout_ordinal,
    })
}
