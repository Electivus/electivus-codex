use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::strip_user_message_prefix;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutItem;
use serde_json::Value;
use sqlx::Postgres;
use sqlx::QueryBuilder;
use sqlx::Row;

use super::PostgresThreadStore;
use super::PostgresThreadTables;
use super::database_error;
use super::list_threads::format_cursor;
use super::list_threads::parse_cursor;
use super::serialization_error;
use crate::SearchThreadsParams;
use crate::SortDirection;
use crate::StoredThreadSearchResult;
use crate::ThreadSearchPage;
use crate::ThreadSortKey;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn search_threads(
    store: &PostgresThreadStore,
    params: SearchThreadsParams,
) -> ThreadStoreResult<ThreadSearchPage> {
    if params.search_term.is_empty() {
        return Err(ThreadStoreError::InvalidRequest {
            message: "thread/search requires search_term".to_string(),
        });
    }
    let cursor = params.cursor.as_deref().map(parse_cursor).transpose()?;
    let limit: i64 = params.page_size.saturating_add(1).try_into().map_err(|_| {
        ThreadStoreError::InvalidRequest {
            message: "thread search page size is too large".to_string(),
        }
    })?;
    let sort_column = match params.sort_key {
        ThreadSortKey::CreatedAt => "created_at",
        ThreadSortKey::UpdatedAt => "updated_at",
        ThreadSortKey::RecencyAt => "recency_at",
        ThreadSortKey::SectionPosition => {
            return Err(ThreadStoreError::InvalidRequest {
                message: "thread search does not support section-position ordering".to_string(),
            });
        }
    };
    let (operator, direction) = match params.sort_direction {
        SortDirection::Asc => (">", "ASC"),
        SortDirection::Desc => ("<", "DESC"),
    };
    let folded_search_term = params.search_term.to_lowercase();
    let mut transaction = store
        .pool
        .begin()
        .await
        .map_err(|error| database_error("search threads", error))?;
    rebuild_stale_search_projections(store, &mut transaction).await?;

    let mut query = QueryBuilder::<Postgres>::new(format!(
        "SELECT threads.projection, threads.{sort_column} AS sort_at, matched.normalized_content \
         FROM {} AS threads JOIN LATERAL (SELECT normalized_content FROM {} AS search_content \
         WHERE search_content.thread_id = threads.thread_id \
         AND strpos(search_content.folded_content, ",
        store.tables.threads, store.tables.search_content
    ));
    query.push_bind(&folded_search_term);
    query.push(") > 0 AND strpos(search_content.normalized_folded_content, ");
    query.push_bind(&folded_search_term);
    query.push(
        ") > 0 ORDER BY search_content.rollout_ordinal ASC LIMIT 1) AS matched ON TRUE WHERE ",
    );
    if params.archived {
        query.push("threads.archived_at IS NOT NULL");
    } else {
        query.push("threads.archived_at IS NULL");
    }
    query.push(" AND COALESCE(threads.projection ->> 'preview', '') <> ''");
    if !params.allowed_sources.is_empty() {
        query.push(" AND threads.projection -> 'source' IN (");
        let mut separated = query.separated(", ");
        for source in &params.allowed_sources {
            separated.push_bind(serde_json::to_value(source).map_err(serialization_error)?);
        }
        separated.push_unseparated(")");
    }
    if let Some(cursor) = cursor.as_ref() {
        query.push(" AND (threads.");
        query.push(sort_column);
        query.push(format!(" {operator} "));
        query.push_bind(cursor.timestamp);
        if let Some(thread_id) = cursor.thread_id {
            query.push(" OR (threads.");
            query.push(sort_column);
            query.push(" = ");
            query.push_bind(cursor.timestamp);
            query.push(format!(" AND threads.thread_id {operator} "));
            query.push_bind(thread_id.to_string());
            query.push(")");
        }
        query.push(")");
    }
    query.push(" ORDER BY threads.");
    query.push(sort_column);
    query.push(format!(" {direction}"));
    query.push(format!(", threads.thread_id {direction}"));
    query.push(" LIMIT ");
    query.push_bind(limit);

    let rows = query
        .build()
        .fetch_all(transaction.as_mut())
        .await
        .map_err(|error| database_error("search threads", error))?;
    let mut items = rows
        .into_iter()
        .map(|row| search_result_from_row(row, &params.search_term))
        .collect::<ThreadStoreResult<Vec<_>>>()?;
    let has_more = items.len() > params.page_size;
    if has_more {
        items.pop();
    }
    let next_cursor = has_more
        .then(|| items.last())
        .flatten()
        .map(|(item, timestamp)| format_cursor(*timestamp, Some(item.thread.thread_id)));
    transaction
        .commit()
        .await
        .map_err(|error| database_error("search threads", error))?;
    Ok(ThreadSearchPage {
        items: items.into_iter().map(|(item, _)| item).collect(),
        next_cursor,
    })
}

async fn rebuild_stale_search_projections(
    store: &PostgresThreadStore,
    transaction: &mut sqlx::Transaction<'_, Postgres>,
) -> ThreadStoreResult<()> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT thread_id, stream_version, history_projection_start_ordinal FROM {} \
         WHERE history_projection_version IS DISTINCT FROM stream_version \
         ORDER BY thread_id FOR UPDATE",
        store.tables.threads
    )))
    .fetch_all(transaction.as_mut())
    .await
    .map_err(|error| database_error("search threads", error))?;
    for row in rows {
        let thread_id = row
            .try_get::<String, _>("thread_id")
            .map_err(|error| database_error("search threads", error))?;
        let thread_id =
            ThreadId::from_string(&thread_id).map_err(|error| ThreadStoreError::Internal {
                message: format!("invalid stored thread id: {error}"),
            })?;
        super::projection::rebuild_history_projections(
            &store.tables,
            transaction.as_mut(),
            thread_id,
            row.try_get("stream_version")
                .map_err(|error| database_error("search threads", error))?,
            row.try_get("history_projection_start_ordinal")
                .map_err(|error| database_error("search threads", error))?,
        )
        .await?;
    }
    Ok(())
}

fn search_result_from_row(
    row: sqlx::postgres::PgRow,
    search_term: &str,
) -> ThreadStoreResult<(StoredThreadSearchResult, DateTime<Utc>)> {
    let projection: Value = row
        .try_get("projection")
        .map_err(|error| database_error("search threads", error))?;
    let thread = serde_json::from_value(projection).map_err(serialization_error)?;
    let content: String = row
        .try_get("normalized_content")
        .map_err(|error| database_error("search threads", error))?;
    let snippet = codex_rollout::thread_search_match_snippet(content.as_str(), search_term)
        .ok_or_else(|| ThreadStoreError::Internal {
            message: "thread search projection returned a non-matching row".to_string(),
        })?;
    let sort_at = row
        .try_get("sort_at")
        .map_err(|error| database_error("search threads", error))?;
    Ok((StoredThreadSearchResult { thread, snippet }, sort_at))
}

pub(super) async fn apply_projection(
    tables: &PostgresThreadTables,
    connection: &mut sqlx::PgConnection,
    thread_id: ThreadId,
    rollout_ordinal: i64,
    item: &RolloutItem,
) -> ThreadStoreResult<()> {
    let Some(content) = searchable_content(item) else {
        return Ok(());
    };
    let normalized_content = normalize_content(content.as_str());
    let folded_content = content.to_lowercase();
    let normalized_folded_content = normalized_content.to_lowercase();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO {} (thread_id, rollout_ordinal, content, folded_content, normalized_content, normalized_folded_content) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (thread_id, rollout_ordinal) DO UPDATE SET \
         content = EXCLUDED.content, folded_content = EXCLUDED.folded_content, \
         normalized_content = EXCLUDED.normalized_content, normalized_folded_content = EXCLUDED.normalized_folded_content",
        tables.search_content
    )))
    .bind(thread_id.to_string())
    .bind(rollout_ordinal)
    .bind(content)
    .bind(folded_content)
    .bind(normalized_content)
    .bind(normalized_folded_content)
    .execute(&mut *connection)
    .await
    .map_err(|error| database_error("project thread search content", error))?;
    Ok(())
}

fn searchable_content(item: &RolloutItem) -> Option<String> {
    if let Some(content) = codex_rollout::thread_searchable_content(item) {
        return Some(content);
    }
    match item {
        RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => match &event.item {
            TurnItem::UserMessage(message) => nonempty(
                message
                    .content
                    .iter()
                    .filter_map(|input| match input {
                        UserInput::Text { text, .. } => {
                            Some(strip_user_message_prefix(text.as_str()))
                        }
                        UserInput::Image { .. }
                        | UserInput::LocalImage { .. }
                        | UserInput::Audio { .. }
                        | UserInput::LocalAudio { .. }
                        | UserInput::Skill { .. }
                        | UserInput::Mention { .. } => None,
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            TurnItem::AgentMessage(message) => nonempty(
                message
                    .content
                    .iter()
                    .map(|content| match content {
                        AgentMessageContent::Text { text } => text.as_str(),
                    })
                    .collect::<String>(),
            ),
            // Only completed user and agent messages contribute searchable text.
            _ => None,
        },
        _ => None,
    }
}

fn nonempty(text: String) -> Option<String> {
    (!text.trim().is_empty()).then_some(text)
}

fn normalize_content(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
