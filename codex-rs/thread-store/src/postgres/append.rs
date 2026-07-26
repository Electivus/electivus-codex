use chrono::Utc;
use codex_rollout::persisted_rollout_items;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Postgres;
use sqlx::QueryBuilder;
use sqlx::Row;

use super::PostgresThreadStore;
use super::WRITER_LEASE_DURATION;
use super::database_error;
use super::metadata::apply_metadata_patch;
use super::serialization_error;
use super::writer_conflict;
use crate::AppendBatchCommit;
use crate::AppendBatchId;
use crate::AppendThreadItemsBatch;
use crate::AppendThreadItemsParams;
use crate::StoredThread;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn append_items(
    store: &PostgresThreadStore,
    params: AppendThreadItemsParams,
) -> ThreadStoreResult<()> {
    let history_mode = store
        .live_writers
        .lock()
        .await
        .get(&params.thread_id)
        .map(|writer| writer.history_mode)
        .ok_or(ThreadStoreError::ThreadNotFound {
            thread_id: params.thread_id,
        })?;
    if persisted_rollout_items(params.items.as_slice(), history_mode).is_empty() {
        return Ok(());
    }
    append_batch(
        store,
        AppendThreadItemsBatch::new(params.thread_id, AppendBatchId::new(), params.items),
    )
    .await?;
    Ok(())
}

pub(super) async fn append_batch(
    store: &PostgresThreadStore,
    batch: AppendThreadItemsBatch,
) -> ThreadStoreResult<AppendBatchCommit> {
    let mut writer = store
        .live_writers
        .lock()
        .await
        .get(&batch.thread_id)
        .cloned()
        .ok_or(ThreadStoreError::ThreadNotFound {
            thread_id: batch.thread_id,
        })?;
    let items = persisted_rollout_items(batch.items.as_slice(), writer.history_mode);
    if items.is_empty() {
        return Err(ThreadStoreError::InvalidRequest {
            message: "append batch contains no durable thread history items".to_string(),
        });
    }
    let content_identity = canonical_content_identity(items.as_slice())?;
    let recorded_at = super::metadata::postgres_timestamp(Utc::now());
    let item_values = items
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(serialization_error)?;
    let mut transaction = store
        .pool
        .begin()
        .await
        .map_err(|error| database_error("append thread history", error))?;
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT projection, stream_version, history_projection_version, fencing_token, writer_id \
         FROM {} WHERE thread_id = $1 FOR UPDATE",
        store.tables.threads
    )))
    .bind(batch.thread_id.to_string())
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error("append thread history", error))?
    .ok_or(ThreadStoreError::ThreadNotFound {
        thread_id: batch.thread_id,
    })?;
    let stream_version: i64 = row
        .try_get("stream_version")
        .map_err(|error| database_error("append thread history", error))?;
    let fencing_token: i64 = row
        .try_get("fencing_token")
        .map_err(|error| database_error("append thread history", error))?;
    let writer_id: String = row
        .try_get("writer_id")
        .map_err(|error| database_error("append thread history", error))?;
    // The current writer may continue after an idle lease expiry if no takeover occurred.
    // A takeover changes both the writer id and fencing token, so the former writer remains fenced.
    if fencing_token != writer.fencing_token || writer_id != store.writer_id {
        return Err(writer_conflict(batch.thread_id));
    }
    let committed_batch = sqlx::query(AssertSqlSafe(format!(
        "SELECT content_identity, first_ordinal, item_count, committed_stream_version \
         FROM {} WHERE thread_id = $1 AND idempotency_key = $2",
        store.tables.append_batches
    )))
    .bind(batch.thread_id.to_string())
    .bind(batch.batch_id.to_string())
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error("append thread history", error))?;
    if let Some(committed_batch) = committed_batch {
        let committed_identity: Vec<u8> = committed_batch
            .try_get("content_identity")
            .map_err(|error| database_error("append thread history", error))?;
        if committed_identity != content_identity {
            return Err(ThreadStoreError::Conflict {
                message: format!(
                    "append batch {} was already committed with different content",
                    batch.batch_id
                ),
            });
        }
        let committed = append_commit_from_row(&committed_batch)?;
        let original_stream_version =
            i64::try_from(committed.first_ordinal).map_err(history_too_large)?;
        if writer.expected_stream_version != original_stream_version
            && writer.expected_stream_version != stream_version
        {
            return Err(writer_conflict(batch.thread_id));
        }
        transaction
            .commit()
            .await
            .map_err(|error| database_error("append thread history", error))?;
        writer.expected_stream_version = stream_version;
        store
            .live_writers
            .lock()
            .await
            .insert(batch.thread_id, writer);
        return Ok(committed);
    }
    if stream_version != writer.expected_stream_version {
        return Err(writer_conflict(batch.thread_id));
    }
    let history_projection_version: Option<i64> = row
        .try_get("history_projection_version")
        .map_err(|error| database_error("append thread history", error))?;
    if history_projection_version != Some(stream_version) {
        super::projection::rebuild_history_projections(
            &store.tables,
            transaction.as_mut(),
            batch.thread_id,
            stream_version,
            writer.history_projection_start_ordinal,
        )
        .await?;
    }
    let projection: Value = row
        .try_get("projection")
        .map_err(|error| database_error("append thread history", error))?;
    let mut projection: StoredThread =
        serde_json::from_value(projection).map_err(serialization_error)?;
    let mut next_metadata_sync = writer.metadata_sync.clone();
    let metadata_update = next_metadata_sync.observe_appended_items(items.as_slice());
    if let Some(update) = metadata_update.as_ref() {
        apply_metadata_patch(&mut projection, &update.patch);
    }
    let item_count = i64::try_from(item_values.len()).map_err(history_too_large)?;
    let committed_stream_version = stream_version
        .checked_add(item_count)
        .ok_or_else(|| history_too_large(()))?;
    let first_ordinal = stream_version;
    let item_rows = item_values
        .into_iter()
        .enumerate()
        .map(|(offset, item)| {
            let offset = i64::try_from(offset).map_err(history_too_large)?;
            let ordinal = first_ordinal
                .checked_add(offset)
                .ok_or_else(|| history_too_large(()))?;
            Ok((ordinal, item, recorded_at))
        })
        .collect::<ThreadStoreResult<Vec<_>>>()?;
    let mut insert = QueryBuilder::<Postgres>::new(format!(
        "INSERT INTO {} (thread_id, ordinal, item, recorded_at) ",
        store.tables.history
    ));
    insert.push_values(item_rows, |mut values, (ordinal, item, recorded_at)| {
        values
            .push_bind(batch.thread_id.to_string())
            .push_bind(ordinal)
            .push_bind(item)
            .push_bind(recorded_at);
    });
    insert
        .build()
        .execute(transaction.as_mut())
        .await
        .map_err(|error| database_error("append thread history", error))?;
    super::projection::apply_history_projections(
        &store.tables,
        transaction.as_mut(),
        batch.thread_id,
        first_ordinal,
        recorded_at,
        writer.history_projection_start_ordinal,
        items.as_slice(),
    )
    .await?;
    let projection_json = serde_json::to_value(&projection).map_err(serialization_error)?;
    let lease_millis =
        i64::try_from(WRITER_LEASE_DURATION.as_millis()).map_err(history_too_large)?;
    let updated = sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET projection = $1, stream_version = $2, history_projection_version = $2, \
         updated_at = $3, recency_at = $4, is_pinned = $5, \
         writer_lease_expires_at = CURRENT_TIMESTAMP + $6 * INTERVAL '1 millisecond' \
         WHERE thread_id = $7 AND writer_id = $8 AND fencing_token = $9 AND stream_version = $10",
        store.tables.threads
    )))
    .bind(projection_json)
    .bind(committed_stream_version)
    .bind(projection.updated_at)
    .bind(projection.recency_at)
    .bind(projection.is_pinned)
    .bind(lease_millis)
    .bind(batch.thread_id.to_string())
    .bind(&store.writer_id)
    .bind(writer.fencing_token)
    .bind(stream_version)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| database_error("append thread history", error))?;
    if updated.rows_affected() != 1 {
        return Err(writer_conflict(batch.thread_id));
    }
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {} (thread_id, idempotency_key, content_identity, first_ordinal, item_count, committed_stream_version, committed_at) \
         VALUES ($1, $2, $3, $4, $5, $6, CURRENT_TIMESTAMP)",
        store.tables.append_batches
    )))
    .bind(batch.thread_id.to_string())
    .bind(batch.batch_id.to_string())
    .bind(content_identity)
    .bind(first_ordinal)
    .bind(item_count)
    .bind(committed_stream_version)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| database_error("append thread history", error))?;
    transaction
        .commit()
        .await
        .map_err(|error| database_error("append thread history", error))?;
    if let Some(update) = metadata_update.as_ref() {
        next_metadata_sync.mark_pending_update_applied(update);
    }
    writer.metadata_sync = next_metadata_sync;
    writer.expected_stream_version = committed_stream_version;
    store
        .live_writers
        .lock()
        .await
        .insert(batch.thread_id, writer);
    Ok(AppendBatchCommit {
        first_ordinal: u64::try_from(first_ordinal).map_err(history_too_large)?,
        persisted_item_count: usize::try_from(item_count).map_err(history_too_large)?,
        committed_stream_version: u64::try_from(committed_stream_version)
            .map_err(history_too_large)?,
    })
}

fn canonical_content_identity(
    items: &[codex_protocol::protocol::RolloutItem],
) -> ThreadStoreResult<Vec<u8>> {
    let mut value = serde_json::to_value(items).map_err(serialization_error)?;
    sort_json_objects(&mut value);
    serde_json::to_vec(&value).map_err(serialization_error)
}

fn sort_json_objects(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(sort_json_objects),
        Value::Object(values) => {
            let mut entries = std::mem::take(values).into_iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            for (key, mut value) in entries {
                sort_json_objects(&mut value);
                values.insert(key, value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn append_commit_from_row(row: &sqlx::postgres::PgRow) -> ThreadStoreResult<AppendBatchCommit> {
    let first_ordinal: i64 = row
        .try_get("first_ordinal")
        .map_err(|error| database_error("append thread history", error))?;
    let item_count: i64 = row
        .try_get("item_count")
        .map_err(|error| database_error("append thread history", error))?;
    let committed_stream_version: i64 = row
        .try_get("committed_stream_version")
        .map_err(|error| database_error("append thread history", error))?;
    Ok(AppendBatchCommit {
        first_ordinal: u64::try_from(first_ordinal).map_err(history_too_large)?,
        persisted_item_count: usize::try_from(item_count).map_err(history_too_large)?,
        committed_stream_version: u64::try_from(committed_stream_version)
            .map_err(history_too_large)?,
    })
}

fn history_too_large<T>(_value: T) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: "thread history is too large to persist".to_string(),
    }
}
