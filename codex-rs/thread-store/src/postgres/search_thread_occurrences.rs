use codex_app_server_protocol::ThreadItem;
use codex_protocol::ThreadId;
use futures::TryStreamExt;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Row;

use super::PostgresThreadStore;
use super::database_error;
use super::items::CursorScope;
use super::items::serialize_cursor;
use super::projection::begin_consistent_read;
use super::serialization_error;
use crate::SearchThreadOccurrencesParams;
use crate::ThreadOccurrenceSearchPage;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::occurrence_search::LiteralMatcher;
use crate::occurrence_search::occurrence_in_item;
use crate::occurrence_search::searchable_text;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchCursor {
    thread_id: ThreadId,
    search_term: String,
    next_rollout_ordinal: i64,
    next_occurrence_index: usize,
}

struct CandidateRow {
    turn_id: String,
    item_id: String,
    rollout_ordinal: i64,
    item: Value,
    turn_rollout_ordinal: i64,
}

pub(super) async fn search_thread_occurrences(
    store: &PostgresThreadStore,
    params: SearchThreadOccurrencesParams,
) -> ThreadStoreResult<ThreadOccurrenceSearchPage> {
    validate_params(&params)?;
    let mut transaction = begin_consistent_read(
        store,
        params.thread_id,
        /*include_archived*/ true,
        "thread/searchOccurrences",
    )
    .await?;
    let cursor = parse_cursor(
        params.cursor.as_deref(),
        params.thread_id,
        &params.search_term,
    )?;
    let next_rollout_ordinal = cursor
        .as_ref()
        .map_or(0, |cursor| cursor.next_rollout_ordinal);
    let matcher = LiteralMatcher::new(params.search_term.as_str());
    let items_table = &store.tables.items;
    let turns_table = &store.tables.turns;
    let mut rows = sqlx::query(AssertSqlSafe(format!(
        "SELECT turn_id, item_id, rollout_ordinal, item, turn_rollout_ordinal FROM ( \
            SELECT items.turn_id, items.item_id, items.rollout_ordinal, items.item, \
                turns.rollout_ordinal AS turn_rollout_ordinal \
            FROM {items_table} AS items JOIN {turns_table} AS turns \
              ON turns.thread_id = items.thread_id AND turns.turn_id = items.turn_id \
            WHERE items.thread_id = $1 AND items.item->>'type' = 'userMessage' \
              AND items.rollout_ordinal >= $2 \
            UNION ALL \
            SELECT items.turn_id, items.item_id, items.rollout_ordinal, items.item, \
                turns.rollout_ordinal AS turn_rollout_ordinal \
            FROM {turns_table} AS turns JOIN LATERAL ( \
                SELECT candidate.turn_id, candidate.item_id, candidate.rollout_ordinal, candidate.item \
                FROM {items_table} AS candidate \
                WHERE candidate.thread_id = turns.thread_id AND candidate.turn_id = turns.turn_id \
                  AND candidate.item->>'type' = 'agentMessage' \
                  AND (candidate.item->>'phase' = 'final_answer' OR ( \
                    turns.status IN ('completed', 'interrupted', 'failed') \
                    AND candidate.item->>'phase' IS NULL)) \
                ORDER BY (candidate.item->>'phase' = 'final_answer') DESC NULLS LAST, \
                    candidate.rollout_ordinal DESC LIMIT 1 \
            ) AS items ON TRUE \
            WHERE turns.thread_id = $1 AND items.rollout_ordinal >= $2 \
        ) AS candidates ORDER BY rollout_ordinal ASC"
    )))
    .bind(params.thread_id.to_string())
    .bind(next_rollout_ordinal)
    .fetch(transaction.as_mut());

    let mut items = Vec::with_capacity(params.page_size);
    let mut next_cursor = None;
    'candidates: while let Some(row) = rows.try_next().await.map_err(search_error)? {
        let row = candidate_row(row)?;
        let item = serde_json::from_value::<ThreadItem>(row.item).map_err(|err| {
            ThreadStoreError::Internal {
                message: format!("failed to deserialize stored thread item: {err}"),
            }
        })?;
        let Some(text) = searchable_text(&item) else {
            continue;
        };
        let first_occurrence_index = cursor
            .as_ref()
            .filter(|cursor| cursor.next_rollout_ordinal == row.rollout_ordinal)
            .map_or(0, |cursor| cursor.next_occurrence_index);
        let remaining = params
            .page_size
            .saturating_add(1)
            .saturating_sub(items.len());
        let turn_cursor = serialize_cursor(
            params.thread_id,
            CursorScope::Turns,
            row.turn_rollout_ordinal,
            /*include_anchor*/ true,
        )?;
        for (occurrence_index, matched) in matcher
            .find_ranges(
                text.as_ref(),
                first_occurrence_index.saturating_add(remaining),
            )
            .into_iter()
            .enumerate()
            .skip(first_occurrence_index)
        {
            if items.len() == params.page_size {
                next_cursor = Some(serialize_search_cursor(SearchCursor {
                    thread_id: params.thread_id,
                    search_term: params.search_term.clone(),
                    next_rollout_ordinal: row.rollout_ordinal,
                    next_occurrence_index: occurrence_index,
                })?);
                break 'candidates;
            }
            items.push(occurrence_in_item(
                row.turn_id.as_str(),
                row.item_id.as_str(),
                text.as_ref(),
                matched,
                turn_cursor.as_str(),
            ));
        }
    }
    drop(rows);
    transaction.commit().await.map_err(search_error)?;
    Ok(ThreadOccurrenceSearchPage { items, next_cursor })
}

fn validate_params(params: &SearchThreadOccurrencesParams) -> ThreadStoreResult<()> {
    if params.search_term.trim().is_empty() {
        return Err(ThreadStoreError::InvalidRequest {
            message: "thread/searchOccurrences requires search_term".to_string(),
        });
    }
    if params.page_size == 0 {
        return Err(ThreadStoreError::InvalidRequest {
            message: "thread/searchOccurrences requires page_size greater than zero".to_string(),
        });
    }
    Ok(())
}

fn candidate_row(row: sqlx::postgres::PgRow) -> ThreadStoreResult<CandidateRow> {
    let rollout_ordinal = row.try_get("rollout_ordinal").map_err(search_error)?;
    let turn_rollout_ordinal = row.try_get("turn_rollout_ordinal").map_err(search_error)?;
    if rollout_ordinal < 0 || turn_rollout_ordinal < 0 {
        return Err(ThreadStoreError::Internal {
            message: "invalid stored thread history ordinal".to_string(),
        });
    }
    Ok(CandidateRow {
        turn_id: row.try_get("turn_id").map_err(search_error)?,
        item_id: row.try_get("item_id").map_err(search_error)?,
        rollout_ordinal,
        item: row.try_get("item").map_err(search_error)?,
        turn_rollout_ordinal,
    })
}

fn parse_cursor(
    cursor: Option<&str>,
    thread_id: ThreadId,
    search_term: &str,
) -> ThreadStoreResult<Option<SearchCursor>> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let value = serde_json::from_str::<SearchCursor>(cursor).map_err(|_| invalid_cursor(cursor))?;
    if value.thread_id != thread_id
        || value.search_term != search_term
        || value.next_rollout_ordinal < 0
    {
        return Err(invalid_cursor(cursor));
    }
    Ok(Some(value))
}

fn serialize_search_cursor(cursor: SearchCursor) -> ThreadStoreResult<String> {
    serde_json::to_string(&cursor).map_err(serialization_error)
}

fn invalid_cursor(cursor: &str) -> ThreadStoreError {
    ThreadStoreError::InvalidRequest {
        message: format!("invalid cursor: {cursor}"),
    }
}

fn search_error(error: sqlx::Error) -> ThreadStoreError {
    database_error("search thread occurrences", error)
}
