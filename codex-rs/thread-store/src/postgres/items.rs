use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sqlx::Postgres;
use sqlx::QueryBuilder;
use sqlx::Row;

use super::PostgresThreadStore;
use super::database_error;
use super::projection::begin_consistent_read;
use super::serialization_error;
use crate::ItemPage;
use crate::ItemSortKey;
use crate::ListItemsParams;
use crate::SortDirection;
use crate::StoredThreadItem;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HistoryCursor {
    requested_thread_id: ThreadId,
    rollout_ordinal: i64,
    include_anchor: bool,
    scope: CursorScope,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(super) enum CursorScope {
    Turns,
    ItemsByCreatedAtOrdinal,
    ItemsByUpdatedAtOrdinal,
}

pub(super) struct StoredThreadItemRow {
    pub(super) item: StoredThreadItem,
    pub(super) rollout_ordinal: i64,
}

pub(super) async fn list_items(
    store: &PostgresThreadStore,
    params: ListItemsParams,
) -> ThreadStoreResult<ItemPage> {
    let mut transaction = begin_consistent_read(
        store,
        params.thread_id,
        params.include_archived,
        "list_items",
    )
    .await?;
    let (cursor_scope, sort_column) = match params.sort_key {
        ItemSortKey::CreatedAtOrdinal => (CursorScope::ItemsByCreatedAtOrdinal, "rollout_ordinal"),
        ItemSortKey::UpdatedAtOrdinal => {
            if params.after_updated_at_ordinal.is_none() {
                return Err(ThreadStoreError::InvalidRequest {
                    message: "update-ordinal item sorting requires an update watermark".to_string(),
                });
            }
            (CursorScope::ItemsByUpdatedAtOrdinal, "updated_at_ordinal")
        }
    };
    let cursor = parse_cursor(params.cursor.as_deref(), params.thread_id, cursor_scope)?;
    let limit = page_limit(params.page_size)?;
    let mut query = QueryBuilder::<Postgres>::new(format!(
        "SELECT turn_id, item_id, {sort_column} AS rollout_ordinal, updated_at_ordinal, \
         created_at_ms, item FROM {} WHERE thread_id = ",
        store.tables.items,
    ));
    query.push_bind(params.thread_id.to_string());
    if let Some(after_updated_at_ordinal) = params.after_updated_at_ordinal {
        let after_updated_at_ordinal = i64::try_from(after_updated_at_ordinal).map_err(|_| {
            ThreadStoreError::InvalidRequest {
                message: "updated-at ordinal is too large".to_string(),
            }
        })?;
        query
            .push(" AND updated_at_ordinal > ")
            .push_bind(after_updated_at_ordinal);
    }
    if let Some(turn_id) = params.turn_id.as_deref() {
        query.push(" AND turn_id = ").push_bind(turn_id);
    }
    push_pagination_clause(
        &mut query,
        params.sort_direction,
        cursor.as_ref(),
        sort_column,
        limit,
    );
    let rows = query
        .build()
        .fetch_all(transaction.as_mut())
        .await
        .map_err(|error| database_error("list thread items", error))?;
    let mut item_rows = rows
        .into_iter()
        .map(stored_thread_item_row)
        .collect::<ThreadStoreResult<Vec<_>>>()?;
    let has_more = item_rows.len() > params.page_size;
    item_rows.truncate(params.page_size);
    let (next_cursor, backwards_cursor) = page_cursors(
        params.thread_id,
        cursor_scope,
        item_rows.first().map(|row| row.rollout_ordinal),
        item_rows.last().map(|row| row.rollout_ordinal),
        has_more,
    )?;
    let page = ItemPage {
        items: item_rows.into_iter().map(|row| row.item).collect(),
        next_cursor,
        backwards_cursor,
    };
    transaction
        .commit()
        .await
        .map_err(|error| database_error("list thread items", error))?;
    Ok(page)
}

pub(super) fn page_limit(page_size: usize) -> ThreadStoreResult<i64> {
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

pub(super) fn parse_cursor(
    cursor: Option<&str>,
    thread_id: ThreadId,
    scope: CursorScope,
) -> ThreadStoreResult<Option<HistoryCursor>> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let cursor_value: HistoryCursor =
        serde_json::from_str(cursor).map_err(|_| invalid_cursor(cursor))?;
    if cursor_value.requested_thread_id != thread_id || cursor_value.scope != scope {
        return Err(invalid_cursor(cursor));
    }
    Ok(Some(cursor_value))
}

pub(super) fn push_pagination_clause(
    query: &mut QueryBuilder<Postgres>,
    direction: SortDirection,
    cursor: Option<&HistoryCursor>,
    sort_column: &'static str,
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
            .push(" AND ")
            .push(sort_column)
            .push(" ")
            .push(comparator)
            .push(" ")
            .push_bind(cursor.rollout_ordinal);
    }
    let order = match direction {
        SortDirection::Asc => "ASC",
        SortDirection::Desc => "DESC",
    };
    query
        .push(" ORDER BY ")
        .push(sort_column)
        .push(" ")
        .push(order)
        .push(" LIMIT ")
        .push_bind(limit);
}

pub(super) fn serialize_cursor(
    thread_id: ThreadId,
    scope: CursorScope,
    rollout_ordinal: i64,
    include_anchor: bool,
) -> ThreadStoreResult<String> {
    serde_json::to_string(&HistoryCursor {
        requested_thread_id: thread_id,
        rollout_ordinal,
        include_anchor,
        scope,
    })
    .map_err(serialization_error)
}

pub(super) fn page_cursors(
    thread_id: ThreadId,
    scope: CursorScope,
    first_ordinal: Option<i64>,
    last_ordinal: Option<i64>,
    has_more: bool,
) -> ThreadStoreResult<(Option<String>, Option<String>)> {
    let cursor = |rollout_ordinal, include_anchor| {
        serialize_cursor(thread_id, scope, rollout_ordinal, include_anchor)
    };
    let backwards_cursor = first_ordinal
        .map(|ordinal| cursor(ordinal, /*include_anchor*/ true))
        .transpose()?;
    let next_cursor = if has_more {
        last_ordinal
            .map(|ordinal| cursor(ordinal, /*include_anchor*/ false))
            .transpose()?
    } else {
        None
    };
    Ok((next_cursor, backwards_cursor))
}

fn invalid_cursor(cursor: &str) -> ThreadStoreError {
    ThreadStoreError::InvalidRequest {
        message: format!("invalid cursor: {cursor}"),
    }
}

pub(super) fn stored_thread_item_row(
    row: sqlx::postgres::PgRow,
) -> ThreadStoreResult<StoredThreadItemRow> {
    let rollout_ordinal = row
        .try_get::<i64, _>("rollout_ordinal")
        .map_err(|error| database_error("list thread items", error))?;
    let item = row
        .try_get::<Value, _>("item")
        .map_err(|error| database_error("list thread items", error))?;
    let updated_at_ordinal = row
        .try_get::<i64, _>("updated_at_ordinal")
        .map_err(|error| database_error("list thread items", error))?;
    let updated_at_ordinal =
        u64::try_from(updated_at_ordinal).map_err(|_| ThreadStoreError::Internal {
            message: format!("invalid stored item updated-at ordinal: {updated_at_ordinal}"),
        })?;
    Ok(StoredThreadItemRow {
        item: StoredThreadItem {
            turn_id: row
                .try_get("turn_id")
                .map_err(|error| database_error("list thread items", error))?,
            item_id: row
                .try_get("item_id")
                .map_err(|error| database_error("list thread items", error))?,
            updated_at_ordinal,
            created_at_ms: row
                .try_get("created_at_ms")
                .map_err(|error| database_error("list thread items", error))?,
            item_json: serde_json::to_vec(&item).map_err(serialization_error)?,
        },
        rollout_ordinal,
    })
}
