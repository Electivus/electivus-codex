use codex_protocol::ThreadId;
use futures::TryStreamExt;
use serde_json::Value;
use sqlx::AssertSqlSafe;

use super::PostgresThreadTables;
use super::database_error;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn validate_migrated_thread_projections(
    tables: &PostgresThreadTables,
    connection: &mut sqlx::PgConnection,
    thread: &codex_state::ThreadMigrationSnapshot,
) -> ThreadStoreResult<()> {
    let thread_id = thread.metadata().id;
    let Some(projection_state) = thread.projection_state() else {
        if thread.turns().is_empty() && thread.items().is_empty() {
            return Ok(());
        }
        return Err(ThreadStoreError::Internal {
            message: format!(
                "local thread history projection for {thread_id} has rows without a cursor"
            ),
        });
    };
    let history_len = u64::try_from(thread.canonical_history().lines().len()).map_err(|_| {
        ThreadStoreError::Internal {
            message: format!("canonical history for {thread_id} is too large"),
        }
    })?;
    if projection_state.next_rollout_ordinal() != history_len {
        return Err(ThreadStoreError::Internal {
            message: format!(
                "local thread history projection cursor for {thread_id} is stale: expected {history_len}, found {}",
                projection_state.next_rollout_ordinal()
            ),
        });
    }
    let item_table = &tables.items;
    validate_projection(
        connection,
        thread_id,
        format!(
            "SELECT (to_jsonb(turns) - 'thread_id') || jsonb_build_object( \
             'first_user_item_id', \
             (SELECT item_id FROM {item_table} AS items WHERE items.thread_id = turns.thread_id \
              AND items.turn_id = turns.turn_id AND items.item->>'type' = 'userMessage' \
              ORDER BY rollout_ordinal LIMIT 1), 'final_agent_item_id', \
             (SELECT item_id FROM {item_table} AS items WHERE items.thread_id = turns.thread_id \
              AND items.turn_id = turns.turn_id AND items.item->>'type' = 'agentMessage' \
              AND (items.item->>'phase' = 'final_answer' OR (turns.status IN \
              ('completed', 'interrupted', 'failed') AND items.item->>'phase' IS NULL)) \
              ORDER BY (items.item->>'phase' = 'final_answer') DESC NULLS LAST, \
              rollout_ordinal DESC LIMIT 1)) FROM {} AS turns WHERE turns.thread_id = $1 \
             ORDER BY turns.rollout_ordinal, turns.turn_id",
            tables.turns
        ),
        thread.turns().iter().map(serde_json::to_value),
    )
    .await?;
    validate_projection(
        connection,
        thread_id,
        format!(
            "SELECT (to_jsonb(items) - 'thread_id') || jsonb_build_object( \
             'item_type', item->>'type') FROM {} AS items WHERE thread_id = $1 \
             ORDER BY rollout_ordinal, turn_id, item_id",
            tables.items
        ),
        thread.items().iter().map(serde_json::to_value),
    )
    .await
}

async fn validate_projection(
    connection: &mut sqlx::PgConnection,
    thread_id: ThreadId,
    query: String,
    expected: impl Iterator<Item = Result<Value, serde_json::Error>>,
) -> ThreadStoreResult<()> {
    let mut actual = sqlx::query_scalar::<_, Value>(AssertSqlSafe(query))
        .bind(thread_id.to_string())
        .fetch(&mut *connection);
    for expected in expected {
        let expected = expected.map_err(|error| ThreadStoreError::Internal {
            message: format!("serialize local thread projection: {error}"),
        })?;
        if actual
            .try_next()
            .await
            .map_err(projection_validation_error)?
            != Some(expected)
        {
            return Err(stale_local_projection(thread_id));
        }
    }
    if actual
        .try_next()
        .await
        .map_err(projection_validation_error)?
        .is_some()
    {
        return Err(stale_local_projection(thread_id));
    }
    Ok(())
}

fn stale_local_projection(thread_id: ThreadId) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!(
            "local thread history projection rows for {thread_id} differ from eager Canonical Thread History projection"
        ),
    }
}

fn projection_validation_error(error: sqlx::Error) -> ThreadStoreError {
    database_error("validate migrated thread projections", error)
}
