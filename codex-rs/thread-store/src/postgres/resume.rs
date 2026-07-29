use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Row;

use super::ActiveWriter;
use super::PostgresThreadStore;
use super::database_error;
use super::lifecycle::lease_millis;
use crate::ResumeThreadParams;
use crate::StoredThread;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::thread_metadata_sync::ThreadMetadataSync;

pub(super) async fn resume_thread(
    store: &PostgresThreadStore,
    params: ResumeThreadParams,
) -> ThreadStoreResult<()> {
    let mut transaction = store
        .pool
        .begin()
        .await
        .map_err(|error| database_error("resume thread", error))?;
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT projection, stream_version, fencing_token, history_projection_start_ordinal, \
         writer_lease_expires_at > CURRENT_TIMESTAMP AS lease_active \
         FROM {} WHERE thread_id = $1 FOR UPDATE",
        store.tables.threads
    )))
    .bind(params.thread_id.to_string())
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error("resume thread", error))?
    .ok_or(ThreadStoreError::ThreadNotFound {
        thread_id: params.thread_id,
    })?;
    let lease_active: bool = row
        .try_get("lease_active")
        .map_err(|error| database_error("resume thread", error))?;
    if lease_active {
        return Err(ThreadStoreError::Conflict {
            message: format!("thread {} already has an active writer", params.thread_id),
        });
    }
    let projection: Value = row
        .try_get("projection")
        .map_err(|error| database_error("resume thread", error))?;
    let projection: StoredThread =
        serde_json::from_value(projection).map_err(super::serialization_error)?;
    if !params.include_archived && projection.archived_at.is_some() {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("thread {} is archived", params.thread_id),
        });
    }
    let stream_version: i64 = row
        .try_get("stream_version")
        .map_err(|error| database_error("resume thread", error))?;
    let fencing_token: i64 = row
        .try_get("fencing_token")
        .map_err(|error| database_error("resume thread", error))?;
    let next_fencing_token =
        fencing_token
            .checked_add(1)
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "thread writer fencing token is exhausted".to_string(),
            })?;
    let acquired = sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET fencing_token = $1, writer_id = $2, \
         writer_lease_expires_at = CURRENT_TIMESTAMP + $3 * INTERVAL '1 millisecond' \
         WHERE thread_id = $4 AND fencing_token = $5 \
         AND writer_lease_expires_at <= CURRENT_TIMESTAMP",
        store.tables.threads
    )))
    .bind(next_fencing_token)
    .bind(&store.writer_id)
    .bind(lease_millis()?)
    .bind(params.thread_id.to_string())
    .bind(fencing_token)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| database_error("resume thread", error))?;
    if acquired.rows_affected() != 1 {
        return Err(ThreadStoreError::Conflict {
            message: format!("thread {} already has an active writer", params.thread_id),
        });
    }
    transaction
        .commit()
        .await
        .map_err(|error| database_error("resume thread", error))?;
    let writer = ActiveWriter {
        fencing_token: next_fencing_token,
        expected_stream_version: stream_version,
        history_mode: projection.history_mode,
        history_projection_start_ordinal: row
            .try_get("history_projection_start_ordinal")
            .map_err(|error| database_error("resume thread", error))?,
        metadata_sync: ThreadMetadataSync::from_stored_thread(&projection),
    };
    store
        .live_writers
        .lock()
        .await
        .insert(params.thread_id, writer);
    Ok(())
}
