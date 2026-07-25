use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Postgres;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Transaction;

use super::PostgresThreadStore;
use super::database_error;
use super::items::CursorScope;
use super::items::page_cursors;
use super::items::page_limit;
use super::items::parse_cursor;
use super::items::push_pagination_clause;
use super::items::stored_thread_item_row;
use super::projection::begin_consistent_read;
use super::serialization_error;
use crate::ListTurnsParams;
use crate::StoredTurn;
use crate::StoredTurnError;
use crate::StoredTurnItemsView;
use crate::StoredTurnStatus;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::TurnPage;

struct StoredTurnRow {
    turn_id: String,
    rollout_ordinal: i64,
    status: StoredTurnStatus,
    error: Option<StoredTurnError>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
}

pub(super) async fn list_turns(
    store: &PostgresThreadStore,
    params: ListTurnsParams,
) -> ThreadStoreResult<TurnPage> {
    let mut transaction = begin_consistent_read(
        store,
        params.thread_id,
        params.include_archived,
        "list_turns",
    )
    .await?;
    let cursor = parse_cursor(
        params.cursor.as_deref(),
        params.thread_id,
        CursorScope::Turns,
    )?;
    let limit = page_limit(params.page_size)?;
    let mut query = QueryBuilder::<Postgres>::new(format!(
        "SELECT turn_id, rollout_ordinal, status, error, started_at, completed_at, duration_ms \
         FROM {} WHERE thread_id = ",
        store.tables.turns
    ));
    query.push_bind(params.thread_id.to_string());
    push_pagination_clause(
        &mut query,
        params.sort_direction,
        cursor.as_ref(),
        "rollout_ordinal",
        limit,
    );
    let rows = query
        .build()
        .fetch_all(transaction.as_mut())
        .await
        .map_err(turn_read_error)?;
    let mut turns = rows
        .into_iter()
        .map(stored_turn_row)
        .collect::<ThreadStoreResult<Vec<_>>>()?;
    let has_more = turns.len() > params.page_size;
    turns.truncate(params.page_size);
    let (next_cursor, backwards_cursor) = page_cursors(
        params.thread_id,
        CursorScope::Turns,
        turns.first().map(|turn| turn.rollout_ordinal),
        turns.last().map(|turn| turn.rollout_ordinal),
        has_more,
    )?;
    let mut stored_turns = Vec::with_capacity(turns.len());
    for turn in turns {
        let items = match params.items_view {
            StoredTurnItemsView::NotLoaded => Vec::new(),
            StoredTurnItemsView::Summary => {
                load_summary_items(store, &mut transaction, params.thread_id, &turn).await?
            }
        };
        stored_turns.push(StoredTurn {
            turn_id: turn.turn_id,
            items,
            items_view: params.items_view,
            status: turn.status,
            error: turn.error,
            started_at: turn.started_at,
            completed_at: turn.completed_at,
            duration_ms: turn.duration_ms,
        });
    }
    let page = TurnPage {
        turns: stored_turns,
        next_cursor,
        backwards_cursor,
    };
    transaction
        .commit()
        .await
        .map_err(|error| database_error("list thread turns", error))?;
    Ok(page)
}

async fn load_summary_items(
    store: &PostgresThreadStore,
    transaction: &mut Transaction<'_, Postgres>,
    thread_id: codex_protocol::ThreadId,
    turn: &StoredTurnRow,
) -> ThreadStoreResult<Vec<crate::StoredThreadItem>> {
    let items = &store.tables.items;
    let rows = sqlx::query(AssertSqlSafe(format!(
        "SELECT turn_id, item_id, rollout_ordinal, updated_at_ordinal, created_at_ms, item \
         FROM {items} \
         WHERE thread_id = $1 AND turn_id = $2 AND ( \
            item_id = (SELECT item_id FROM {items} WHERE thread_id = $1 AND turn_id = $2 \
                AND item->>'type' = 'userMessage' ORDER BY rollout_ordinal ASC LIMIT 1) \
            OR item_id = (SELECT item_id FROM {items} WHERE thread_id = $1 AND turn_id = $2 \
                AND item->>'type' = 'agentMessage' \
                AND (item->>'phase' = 'final_answer' OR ($3 AND item->>'phase' IS NULL)) \
                ORDER BY (item->>'phase' = 'final_answer') DESC NULLS LAST, rollout_ordinal DESC LIMIT 1) \
         ) ORDER BY rollout_ordinal ASC"
    )))
    .bind(thread_id.to_string())
    .bind(turn.turn_id.as_str())
    .bind(turn_is_terminal(turn.status))
    .fetch_all(transaction.as_mut())
    .await
    .map_err(|error| database_error("load turn summary items", error))?;
    rows.into_iter()
        .map(|row| stored_thread_item_row(row).map(|item| item.item))
        .collect()
}

fn turn_is_terminal(status: StoredTurnStatus) -> bool {
    match status {
        StoredTurnStatus::Completed | StoredTurnStatus::Interrupted | StoredTurnStatus::Failed => {
            true
        }
        StoredTurnStatus::InProgress => false,
    }
}

fn stored_turn_row(row: sqlx::postgres::PgRow) -> ThreadStoreResult<StoredTurnRow> {
    let status = match row
        .try_get::<String, _>("status")
        .map_err(turn_read_error)?
        .as_str()
    {
        "completed" => StoredTurnStatus::Completed,
        "interrupted" => StoredTurnStatus::Interrupted,
        "failed" => StoredTurnStatus::Failed,
        "inProgress" => StoredTurnStatus::InProgress,
        status => {
            return Err(ThreadStoreError::Internal {
                message: format!("unknown stored turn status: {status}"),
            });
        }
    };
    let error = row
        .try_get::<Option<Value>, _>("error")
        .map_err(turn_read_error)?
        .map(serde_json::from_value)
        .transpose()
        .map_err(serialization_error)?;
    Ok(StoredTurnRow {
        turn_id: row.try_get("turn_id").map_err(turn_read_error)?,
        rollout_ordinal: row.try_get("rollout_ordinal").map_err(turn_read_error)?,
        status,
        error,
        started_at: row.try_get("started_at").map_err(turn_read_error)?,
        completed_at: row.try_get("completed_at").map_err(turn_read_error)?,
        duration_ms: row.try_get("duration_ms").map_err(turn_read_error)?,
    })
}

fn turn_read_error(error: sqlx::Error) -> ThreadStoreError {
    database_error("list thread turns", error)
}
