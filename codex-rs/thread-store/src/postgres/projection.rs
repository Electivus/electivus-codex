use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use codex_app_server_protocol::project_rollout_line;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use sqlx::AssertSqlSafe;
use sqlx::Postgres;
use sqlx::Transaction;

use super::PostgresThreadStore;
use super::database_error;
use super::serialization_error;
use crate::ThreadStoreResult;

pub(super) async fn apply_item_projections(
    store: &PostgresThreadStore,
    transaction: &mut Transaction<'_, Postgres>,
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
        for change in project_rollout_line(&line).changed_items {
            let item_id = change.item.id().to_string();
            let item = serde_json::to_value(change.item).map_err(serialization_error)?;
            sqlx::query(AssertSqlSafe(format!(
                "INSERT INTO {} (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (thread_id, turn_id, item_id) DO UPDATE SET item = EXCLUDED.item",
                store.tables.items
            )))
            .bind(thread_id.to_string())
            .bind(change.turn_id)
            .bind(item_id)
            .bind(rollout_ordinal)
            .bind(recorded_at.timestamp_millis())
            .bind(item)
            .execute(transaction.as_mut())
            .await
            .map_err(|error| database_error("project thread items", error))?;
        }
    }
    Ok(())
}

fn projection_too_large<T>(_value: T) -> crate::ThreadStoreError {
    crate::ThreadStoreError::Internal {
        message: "thread history projection is too large to persist".to_string(),
    }
}
