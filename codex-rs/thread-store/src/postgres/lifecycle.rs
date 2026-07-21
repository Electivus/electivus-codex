use codex_protocol::ThreadId;
use sqlx::AssertSqlSafe;

use super::PostgresThreadStore;
use super::WRITER_LEASE_DURATION;
use super::database_error;
use super::writer_conflict;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn renew_writer(
    store: &PostgresThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    let writer = store
        .live_writers
        .lock()
        .await
        .get(&thread_id)
        .cloned()
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
    let lease_millis = lease_millis()?;
    let renewed = sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET writer_lease_expires_at = CURRENT_TIMESTAMP + $1 * INTERVAL '1 millisecond' \
         WHERE thread_id = $2 AND writer_id = $3 AND fencing_token = $4 AND stream_version = $5 \
         AND writer_lease_expires_at > CURRENT_TIMESTAMP",
        store.tables.threads
    )))
    .bind(lease_millis)
    .bind(thread_id.to_string())
    .bind(&store.writer_id)
    .bind(writer.fencing_token)
    .bind(writer.expected_stream_version)
    .execute(&store.pool)
    .await
    .map_err(|error| database_error("flush thread history", error))?;
    if renewed.rows_affected() != 1 {
        return Err(writer_conflict(thread_id));
    }
    Ok(())
}

pub(super) async fn release_writer(
    store: &PostgresThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    let writer = store
        .live_writers
        .lock()
        .await
        .get(&thread_id)
        .cloned()
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
    let released = sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET writer_lease_expires_at = CURRENT_TIMESTAMP \
         WHERE thread_id = $1 AND writer_id = $2 AND fencing_token = $3 AND stream_version = $4 \
         AND writer_lease_expires_at > CURRENT_TIMESTAMP",
        store.tables.threads
    )))
    .bind(thread_id.to_string())
    .bind(&store.writer_id)
    .bind(writer.fencing_token)
    .bind(writer.expected_stream_version)
    .execute(&store.pool)
    .await
    .map_err(|error| database_error("shutdown thread history", error))?;
    if released.rows_affected() != 1 {
        return Err(writer_conflict(thread_id));
    }
    store.live_writers.lock().await.remove(&thread_id);
    Ok(())
}

fn lease_millis() -> ThreadStoreResult<i64> {
    i64::try_from(WRITER_LEASE_DURATION.as_millis()).map_err(|_| ThreadStoreError::Internal {
        message: "thread writer lease duration is out of range".to_string(),
    })
}
