use chrono::DateTime;
use chrono::NaiveDateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use codex_protocol::ThreadId;
use serde_json::Value;
use sqlx::Postgres;
use sqlx::QueryBuilder;
use sqlx::Row;

use super::PostgresThreadStore;
use super::database_error;
use super::serialization_error;
use crate::ListThreadsParams;
use crate::SortDirection;
use crate::StoredThread;
use crate::ThreadPage;
use crate::ThreadRelationFilter;
use crate::ThreadSortKey;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) struct ListCursor {
    pub(super) timestamp: DateTime<Utc>,
    pub(super) thread_id: Option<ThreadId>,
}

pub(super) async fn list_threads(
    store: &PostgresThreadStore,
    params: ListThreadsParams,
) -> ThreadStoreResult<ThreadPage> {
    let cursor = params.cursor.as_deref().map(parse_cursor).transpose()?;
    let limit: i64 = params.page_size.saturating_add(1).try_into().map_err(|_| {
        ThreadStoreError::InvalidRequest {
            message: "thread list page size is too large".to_string(),
        }
    })?;
    let sort_column = match params.sort_key {
        ThreadSortKey::CreatedAt => "created_at",
        ThreadSortKey::UpdatedAt => "updated_at",
        ThreadSortKey::RecencyAt => "recency_at",
    };
    let (operator, direction) = match params.sort_direction {
        SortDirection::Asc => (">", "ASC"),
        SortDirection::Desc => ("<", "DESC"),
    };
    let include_relation_parent = params.relation_filter.is_some();
    let mut query = QueryBuilder::<Postgres>::new("");
    if let Some(ThreadRelationFilter::DescendantsOf(ancestor_thread_id)) = params.relation_filter {
        query.push(format!(
            "WITH RECURSIVE descendants(thread_id) AS (\
             SELECT child_thread_id FROM {} WHERE parent_thread_id = ",
            store.tables.spawn_edges
        ));
        query.push_bind(ancestor_thread_id.to_string());
        query.push(format!(
            " UNION SELECT edge.child_thread_id FROM {} AS edge \
                 JOIN descendants AS parent ON edge.parent_thread_id = parent.thread_id) ",
            store.tables.spawn_edges
        ));
    }
    query.push(format!(
        "SELECT threads.projection, threads.{sort_column} AS sort_at"
    ));
    if include_relation_parent {
        query.push(format!(
            ", (SELECT edge.parent_thread_id FROM {} AS edge \
             WHERE edge.child_thread_id = threads.thread_id) AS relation_parent_thread_id",
            store.tables.spawn_edges
        ));
    }
    query.push(format!(" FROM {} AS threads", store.tables.threads));
    if matches!(
        params.relation_filter,
        Some(ThreadRelationFilter::DescendantsOf(_))
    ) {
        query.push(" JOIN descendants ON descendants.thread_id = threads.thread_id");
    }
    query.push(" WHERE ");
    if params.archived {
        query.push("threads.archived_at IS NOT NULL");
    } else {
        query.push("threads.archived_at IS NULL");
    }
    query.push(" AND COALESCE(threads.projection ->> 'preview', '') <> ''");
    if let Some(ThreadRelationFilter::DescendantsOf(ancestor_thread_id)) = params.relation_filter {
        query.push(" AND threads.thread_id <> ");
        query.push_bind(ancestor_thread_id.to_string());
    }
    if !params.allowed_sources.is_empty() {
        query.push(" AND threads.projection -> 'source' IN (");
        let mut separated = query.separated(", ");
        for source in &params.allowed_sources {
            separated.push_bind(serde_json::to_value(source).map_err(serialization_error)?);
        }
        separated.push_unseparated(")");
    }
    if let Some(model_providers) = params.model_providers.as_deref()
        && !model_providers.is_empty()
    {
        query.push(" AND threads.projection ->> 'model_provider' IN (");
        let mut separated = query.separated(", ");
        for provider in model_providers {
            separated.push_bind(provider);
        }
        separated.push_unseparated(")");
    }
    match params.cwd_filters.as_deref() {
        Some([]) => {
            query.push(" AND FALSE");
        }
        Some(cwd_filters) => {
            query.push(" AND threads.projection ->> 'cwd' IN (");
            let mut separated = query.separated(", ");
            for cwd in cwd_filters {
                separated.push_bind(cwd.display().to_string());
            }
            separated.push_unseparated(")");
        }
        None => {}
    }
    if let Some(is_pinned) = params.is_pinned {
        query.push(" AND threads.is_pinned = ");
        query.push_bind(is_pinned);
    }
    if let Some(search_term) = params.search_term.as_deref() {
        query.push(" AND (strpos(COALESCE(threads.projection ->> 'name', ''), ");
        query.push_bind(search_term);
        query.push(") > 0 OR strpos(COALESCE(threads.projection ->> 'preview', ''), ");
        query.push_bind(search_term);
        query.push(") > 0)");
    }
    if let Some(ThreadRelationFilter::DirectChildrenOf(parent_thread_id)) = params.relation_filter {
        query.push(format!(
            " AND threads.thread_id IN (\
             SELECT child_thread_id FROM {} WHERE parent_thread_id = ",
            store.tables.spawn_edges
        ));
        query.push_bind(parent_thread_id.to_string());
        query.push(")");
    }
    if let Some(cursor) = cursor.as_ref() {
        query.push(" AND (");
        query.push("threads.");
        query.push(sort_column);
        query.push(format!(" {operator} "));
        query.push_bind(cursor.timestamp);
        if let Some(thread_id) = cursor.thread_id {
            query.push(" OR (");
            query.push("threads.");
            query.push(sort_column);
            query.push(" = ");
            query.push_bind(cursor.timestamp);
            query.push(format!(" AND threads.thread_id {operator} "));
            query.push_bind(thread_id.to_string());
            query.push(")");
        }
        query.push(")");
    }
    query.push(" ORDER BY ");
    query.push("threads.");
    query.push(sort_column);
    query.push(format!(" {direction}"));
    query.push(format!(", threads.thread_id {direction}"));
    query.push(" LIMIT ");
    query.push_bind(limit);

    let rows = query
        .build()
        .fetch_all(&store.pool)
        .await
        .map_err(|error| database_error("list threads", error))?;
    let mut items = rows
        .into_iter()
        .map(|row| {
            let projection: Value = row
                .try_get("projection")
                .map_err(|error| database_error("list threads", error))?;
            let mut thread: StoredThread =
                serde_json::from_value(projection).map_err(serialization_error)?;
            if include_relation_parent {
                let parent_thread_id: String = row
                    .try_get("relation_parent_thread_id")
                    .map_err(|error| database_error("list threads", error))?;
                thread.parent_thread_id =
                    Some(ThreadId::from_string(&parent_thread_id).map_err(|_| {
                        ThreadStoreError::Internal {
                            message: "thread store found an invalid parent thread relationship"
                                .to_string(),
                        }
                    })?);
            }
            let sort_at = row
                .try_get("sort_at")
                .map_err(|error| database_error("list threads", error))?;
            Ok((thread, sort_at))
        })
        .collect::<ThreadStoreResult<Vec<(StoredThread, DateTime<Utc>)>>>()?;
    let has_more = items.len() > params.page_size;
    if has_more {
        items.pop();
    }
    let next_cursor = has_more
        .then(|| items.last())
        .flatten()
        .map(|(thread, timestamp)| format_cursor(*timestamp, Some(thread.thread_id)));
    Ok(ThreadPage {
        items: items.into_iter().map(|(thread, _)| thread).collect(),
        next_cursor,
    })
}

pub(super) fn parse_cursor(cursor: &str) -> ThreadStoreResult<ListCursor> {
    let (timestamp, thread_id) = match cursor.rsplit_once('|') {
        Some((timestamp, thread_id)) => (
            timestamp,
            Some(ThreadId::from_string(thread_id).map_err(|_| invalid_cursor(cursor))?),
        ),
        None => (cursor, None),
    };
    let timestamp = DateTime::parse_from_rfc3339(timestamp)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H-%M-%S")
                .map(|timestamp| timestamp.and_utc())
        })
        .map_err(|_| invalid_cursor(cursor))?;
    Ok(ListCursor {
        timestamp,
        thread_id,
    })
}

pub(super) fn format_cursor(timestamp: DateTime<Utc>, thread_id: Option<ThreadId>) -> String {
    let timestamp = timestamp.to_rfc3339_opts(SecondsFormat::AutoSi, true);
    match thread_id {
        Some(thread_id) => format!("{timestamp}|{thread_id}"),
        None => timestamp,
    }
}

fn invalid_cursor(cursor: &str) -> ThreadStoreError {
    ThreadStoreError::InvalidRequest {
        message: format!("invalid cursor: {cursor}"),
    }
}
