use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use codex_app_server_protocol::ThreadHistoryTurnChange;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::project_rollout_line;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::PgConnection;
use sqlx::Postgres;
use sqlx::Row;
use sqlx::Transaction;
use sqlx::types::Json;

use super::PostgresThreadStore;
use super::PostgresThreadTables;
use super::database_error;
use super::serialization_error;
use crate::StoredThread;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn begin_consistent_read(
    store: &PostgresThreadStore,
    thread_id: codex_protocol::ThreadId,
    include_archived: bool,
    operation: &'static str,
) -> ThreadStoreResult<Transaction<'static, Postgres>> {
    let mut transaction = store
        .pool
        .begin()
        .await
        .map_err(|error| database_error(operation, error))?;
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT projection, stream_version, history_projection_version, \
         history_projection_start_ordinal FROM {} WHERE thread_id = $1 FOR UPDATE",
        store.tables.threads
    )))
    .bind(thread_id.to_string())
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error(operation, error))?
    .ok_or(ThreadStoreError::Unsupported { operation })?;
    let projection = row
        .try_get::<Value, _>("projection")
        .map_err(|error| database_error(operation, error))?;
    let thread: StoredThread = serde_json::from_value(projection).map_err(serialization_error)?;
    if thread.archived_at.is_some() && !include_archived {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("thread {thread_id} is archived"),
        });
    }
    if thread.history_mode == ThreadHistoryMode::Legacy {
        return Err(ThreadStoreError::Unsupported { operation });
    }
    let stream_version = row
        .try_get::<i64, _>("stream_version")
        .map_err(|error| database_error(operation, error))?;
    let projection_version = row
        .try_get::<Option<i64>, _>("history_projection_version")
        .map_err(|error| database_error(operation, error))?;
    if projection_version != Some(stream_version) {
        rebuild_history_projections(
            &store.tables,
            transaction.as_mut(),
            thread_id,
            stream_version,
            row.try_get("history_projection_start_ordinal")
                .map_err(|error| database_error(operation, error))?,
        )
        .await?;
    }
    Ok(transaction)
}

pub(super) async fn rebuild_history_projections(
    tables: &PostgresThreadTables,
    connection: &mut PgConnection,
    thread_id: codex_protocol::ThreadId,
    stream_version: i64,
    history_projection_start_ordinal: Option<i64>,
) -> ThreadStoreResult<()> {
    clear_history_projections(tables, connection, thread_id).await?;
    let mut expected_ordinal = 0_i64;
    loop {
        let rows = sqlx::query_as::<_, (i64, Value, DateTime<Utc>)>(AssertSqlSafe(format!(
            "SELECT ordinal, item, recorded_at FROM {} \
             WHERE thread_id = $1 AND ordinal >= $2 ORDER BY ordinal ASC LIMIT 256",
            tables.history
        )))
        .bind(thread_id.to_string())
        .bind(expected_ordinal)
        .fetch_all(&mut *connection)
        .await
        .map_err(|error| database_error("rebuild thread history projections", error))?;
        if rows.is_empty() {
            break;
        }
        for (ordinal, item, recorded_at) in rows {
            if ordinal != expected_ordinal {
                return Err(invalid_canonical_history(thread_id));
            }
            let item = serde_json::from_value(item).map_err(serialization_error)?;
            apply_history_projections(
                tables,
                connection,
                thread_id,
                ordinal,
                recorded_at,
                history_projection_start_ordinal,
                std::slice::from_ref(&item),
            )
            .await?;
            expected_ordinal = expected_ordinal
                .checked_add(1)
                .ok_or_else(|| projection_too_large(()))?;
        }
    }
    if expected_ordinal != stream_version {
        return Err(invalid_canonical_history(thread_id));
    }
    finish_history_projection(tables, connection, thread_id, stream_version).await
}

pub(super) async fn rebuild_history_projections_from_lines(
    tables: &PostgresThreadTables,
    connection: &mut PgConnection,
    thread_id: codex_protocol::ThreadId,
    stream_version: i64,
    history_projection_start_ordinal: Option<i64>,
    lines: &[RolloutLine],
) -> ThreadStoreResult<()> {
    if i64::try_from(lines.len()).map_err(projection_too_large)? != stream_version {
        return Err(invalid_canonical_history(thread_id));
    }
    clear_history_projections(tables, connection, thread_id).await?;
    for (ordinal, line) in lines.iter().enumerate() {
        let ordinal = i64::try_from(ordinal).map_err(projection_too_large)?;
        let recorded_at = DateTime::parse_from_rfc3339(&line.timestamp)
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("invalid canonical history timestamp: {error}"),
            })?
            .to_utc();
        apply_history_projections(
            tables,
            connection,
            thread_id,
            ordinal,
            recorded_at,
            history_projection_start_ordinal,
            std::slice::from_ref(&line.item),
        )
        .await?;
    }
    finish_history_projection(tables, connection, thread_id, stream_version).await
}

async fn clear_history_projections(
    tables: &PostgresThreadTables,
    connection: &mut PgConnection,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<()> {
    for table in [&tables.items, &tables.turns, &tables.search_content] {
        sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE thread_id = $1"
        )))
        .bind(thread_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(|error| database_error("rebuild thread history projections", error))?;
    }
    Ok(())
}

async fn finish_history_projection(
    tables: &PostgresThreadTables,
    connection: &mut PgConnection,
    thread_id: codex_protocol::ThreadId,
    stream_version: i64,
) -> ThreadStoreResult<()> {
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET history_projection_version = $1 WHERE thread_id = $2",
        tables.threads
    )))
    .bind(stream_version)
    .bind(thread_id.to_string())
    .execute(&mut *connection)
    .await
    .map_err(|error| database_error("rebuild thread history projections", error))?;
    Ok(())
}

pub(super) async fn apply_history_projections(
    tables: &PostgresThreadTables,
    connection: &mut PgConnection,
    thread_id: codex_protocol::ThreadId,
    first_ordinal: i64,
    recorded_at: DateTime<Utc>,
    history_projection_start_ordinal: Option<i64>,
    items: &[RolloutItem],
) -> ThreadStoreResult<()> {
    let timestamp = recorded_at.to_rfc3339_opts(SecondsFormat::Millis, true);
    for (offset, item) in items.iter().enumerate() {
        let offset = i64::try_from(offset).map_err(projection_too_large)?;
        let rollout_ordinal = first_ordinal
            .checked_add(offset)
            .ok_or_else(|| projection_too_large(()))?;
        if let RolloutItem::EventMsg(EventMsg::ThreadRolledBack(event)) = item {
            apply_rollback_projection(tables, connection, thread_id, event.num_turns).await?;
            continue;
        }
        super::search_threads::apply_projection(
            tables,
            connection,
            thread_id,
            rollout_ordinal,
            item,
        )
        .await?;
        if history_projection_start_ordinal
            .is_some_and(|start_ordinal| rollout_ordinal < start_ordinal)
        {
            continue;
        }
        let line = RolloutLine {
            timestamp: timestamp.clone(),
            ordinal: Some(u64::try_from(rollout_ordinal).map_err(projection_too_large)?),
            item: item.clone(),
        };
        let changes = project_rollout_line(&line);
        for change in changes.changed_turns {
            apply_turn_projection(tables, connection, thread_id, rollout_ordinal, change).await?;
        }
        for change in changes.changed_items {
            let created_at_ms = change
                .started_at_ms
                .unwrap_or_else(|| recorded_at.timestamp_millis());
            let item_id = change.item.id().to_string();
            let item = serde_json::to_value(change.item).map_err(serialization_error)?;
            sqlx::query(AssertSqlSafe(format!(
                "INSERT INTO {} (thread_id, turn_id, item_id, rollout_ordinal, \
                 updated_at_ordinal, created_at_ms, item) \
                 VALUES ($1, $2, $3, $4, $4, $5, $6) \
                 ON CONFLICT (thread_id, turn_id, item_id) DO UPDATE SET \
                 updated_at_ordinal = EXCLUDED.updated_at_ordinal, item = EXCLUDED.item",
                tables.items
            )))
            .bind(thread_id.to_string())
            .bind(change.turn_id)
            .bind(item_id)
            .bind(rollout_ordinal)
            .bind(created_at_ms)
            .bind(item)
            .execute(&mut *connection)
            .await
            .map_err(|error| database_error("project thread items", error))?;
        }
    }
    Ok(())
}

async fn apply_rollback_projection(
    tables: &PostgresThreadTables,
    connection: &mut PgConnection,
    thread_id: codex_protocol::ThreadId,
    num_turns: u32,
) -> ThreadStoreResult<()> {
    if num_turns == 0 {
        return Ok(());
    }
    let rollback_start: Option<i64> = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT MIN(rollout_ordinal) FROM (\
            SELECT rollout_ordinal FROM {} WHERE thread_id = $1 \
            ORDER BY rollout_ordinal DESC LIMIT $2\
         ) AS rolled_back_turns",
        tables.turns
    )))
    .bind(thread_id.to_string())
    .bind(i64::from(num_turns))
    .fetch_one(&mut *connection)
    .await
    .map_err(|error| database_error("project thread rollback", error))?;
    let Some(rollback_start) = rollback_start else {
        return Ok(());
    };

    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {} WHERE thread_id = $1 AND rollout_ordinal >= $2",
        tables.search_content
    )))
    .bind(thread_id.to_string())
    .bind(rollback_start)
    .execute(&mut *connection)
    .await
    .map_err(|error| database_error("project thread rollback", error))?;
    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {} WHERE thread_id = $1 AND turn_id IN (\
            SELECT turn_id FROM {} WHERE thread_id = $1 AND rollout_ordinal >= $2\
         )",
        tables.items, tables.turns
    )))
    .bind(thread_id.to_string())
    .bind(rollback_start)
    .execute(&mut *connection)
    .await
    .map_err(|error| database_error("project thread rollback", error))?;
    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {} WHERE thread_id = $1 AND rollout_ordinal >= $2",
        tables.turns
    )))
    .bind(thread_id.to_string())
    .bind(rollback_start)
    .execute(&mut *connection)
    .await
    .map_err(|error| database_error("project thread rollback", error))?;
    Ok(())
}

async fn apply_turn_projection(
    tables: &PostgresThreadTables,
    connection: &mut PgConnection,
    thread_id: codex_protocol::ThreadId,
    rollout_ordinal: i64,
    change: ThreadHistoryTurnChange,
) -> ThreadStoreResult<()> {
    let error = change.error.map(Json);
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {} (thread_id, turn_id, rollout_ordinal, status, error, started_at, completed_at, duration_ms) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (thread_id, turn_id) DO UPDATE SET \
            status = EXCLUDED.status, error = EXCLUDED.error, started_at = EXCLUDED.started_at, \
            completed_at = EXCLUDED.completed_at, duration_ms = EXCLUDED.duration_ms",
        tables.turns
    )))
    .bind(thread_id.to_string())
    .bind(change.turn_id)
    .bind(rollout_ordinal)
    .bind(turn_status(change.status))
    .bind(error)
    .bind(change.started_at)
    .bind(change.completed_at)
    .bind(change.duration_ms)
    .execute(&mut *connection)
    .await
    .map_err(|error| database_error("project thread turns", error))?;
    Ok(())
}

fn turn_status(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Completed => "completed",
        TurnStatus::Interrupted => "interrupted",
        TurnStatus::Failed => "failed",
        TurnStatus::InProgress => "inProgress",
    }
}

fn projection_too_large<T>(_value: T) -> crate::ThreadStoreError {
    crate::ThreadStoreError::Internal {
        message: "thread history projection is too large to persist".to_string(),
    }
}

fn invalid_canonical_history(thread_id: codex_protocol::ThreadId) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("canonical history for thread {thread_id} is not contiguous"),
    }
}
