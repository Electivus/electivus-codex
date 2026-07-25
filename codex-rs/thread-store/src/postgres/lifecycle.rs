use chrono::Utc;
use codex_protocol::ThreadId;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Row;

use super::PostgresThreadStore;
use super::WRITER_LEASE_DURATION;
use super::database_error;
use super::metadata::postgres_timestamp;
use super::serialization_error;
use super::writer_conflict;
use crate::ArchiveThreadParams;
use crate::DeleteThreadParams;
use crate::StoredThread;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn archive_thread(
    store: &PostgresThreadStore,
    params: ArchiveThreadParams,
) -> ThreadStoreResult<()> {
    let mut transaction = store
        .pool
        .begin()
        .await
        .map_err(|error| database_error("archive thread", error))?;
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT projection, fencing_token, archived_at, CURRENT_TIMESTAMP AS database_now FROM {} \
         WHERE thread_id = $1 FOR UPDATE",
        store.tables.threads
    )))
    .bind(params.thread_id.to_string())
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error("archive thread", error))?
    .ok_or_else(|| ThreadStoreError::InvalidRequest {
        message: format!("no rollout found for thread id {}", params.thread_id),
    })?;
    let archived_at: Option<chrono::DateTime<Utc>> = row
        .try_get("archived_at")
        .map_err(|error| database_error("archive thread", error))?;
    if archived_at.is_some() {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("no rollout found for thread id {}", params.thread_id),
        });
    }
    let projection: Value = row
        .try_get("projection")
        .map_err(|error| database_error("archive thread", error))?;
    let mut projection: StoredThread =
        serde_json::from_value(projection).map_err(serialization_error)?;
    let database_now = row
        .try_get("database_now")
        .map_err(|error| database_error("archive thread", error))?;
    let archived_at = postgres_timestamp(database_now);
    projection.archived_at = Some(archived_at);
    let fencing_token: i64 = row
        .try_get("fencing_token")
        .map_err(|error| database_error("archive thread", error))?;
    let next_fencing_token =
        fencing_token
            .checked_add(1)
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "thread writer fencing token is exhausted".to_string(),
            })?;
    let projection = serde_json::to_value(projection).map_err(serialization_error)?;
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET projection = $1, archived_at = $2, fencing_token = $3, \
         writer_lease_expires_at = CURRENT_TIMESTAMP WHERE thread_id = $4",
        store.tables.threads
    )))
    .bind(projection)
    .bind(archived_at)
    .bind(next_fencing_token)
    .bind(params.thread_id.to_string())
    .execute(transaction.as_mut())
    .await
    .map_err(|error| database_error("archive thread", error))?;
    transaction
        .commit()
        .await
        .map_err(|error| database_error("archive thread", error))?;
    store.live_writers.lock().await.remove(&params.thread_id);
    Ok(())
}

pub(super) async fn unarchive_thread(
    store: &PostgresThreadStore,
    params: ArchiveThreadParams,
) -> ThreadStoreResult<StoredThread> {
    let mut transaction = store
        .pool
        .begin()
        .await
        .map_err(|error| database_error("unarchive thread", error))?;
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT projection, archived_at, CURRENT_TIMESTAMP AS database_now FROM {} \
         WHERE thread_id = $1 FOR UPDATE",
        store.tables.threads
    )))
    .bind(params.thread_id.to_string())
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error("unarchive thread", error))?
    .ok_or_else(|| ThreadStoreError::InvalidRequest {
        message: format!(
            "no archived rollout found for thread id {}",
            params.thread_id
        ),
    })?;
    let archived_at: Option<chrono::DateTime<Utc>> = row
        .try_get("archived_at")
        .map_err(|error| database_error("unarchive thread", error))?;
    if archived_at.is_none() {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "no archived rollout found for thread id {}",
                params.thread_id
            ),
        });
    }
    let projection: Value = row
        .try_get("projection")
        .map_err(|error| database_error("unarchive thread", error))?;
    let mut projection: StoredThread =
        serde_json::from_value(projection).map_err(serialization_error)?;
    projection.archived_at = None;
    let database_now = row
        .try_get("database_now")
        .map_err(|error| database_error("unarchive thread", error))?;
    projection.updated_at = projection.updated_at.max(postgres_timestamp(database_now));
    let projection_json = serde_json::to_value(&projection).map_err(serialization_error)?;
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET projection = $1, updated_at = $2, archived_at = NULL \
         WHERE thread_id = $3",
        store.tables.threads
    )))
    .bind(projection_json)
    .bind(projection.updated_at)
    .bind(params.thread_id.to_string())
    .execute(transaction.as_mut())
    .await
    .map_err(|error| database_error("unarchive thread", error))?;
    transaction
        .commit()
        .await
        .map_err(|error| database_error("unarchive thread", error))?;
    Ok(projection)
}

pub(super) async fn delete_thread(
    store: &PostgresThreadStore,
    params: DeleteThreadParams,
) -> ThreadStoreResult<()> {
    if let Some(state_db) = store.state_db.as_ref() {
        let deleted = state_db
            .delete_thread(params.thread_id)
            .await
            .map_err(|_| ThreadStoreError::Internal {
                message: "thread store could not complete `delete thread`; verify persistence health, then retry"
                    .to_string(),
            })?;
        if deleted != 1 {
            return Err(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            });
        }
        store.live_writers.lock().await.remove(&params.thread_id);
        return Ok(());
    }

    let mut transaction = store
        .pool
        .begin()
        .await
        .map_err(|error| database_error("delete thread", error))?;
    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {} WHERE parent_thread_id = $1 OR child_thread_id = $1",
        store.tables.spawn_edges
    )))
    .bind(params.thread_id.to_string())
    .execute(transaction.as_mut())
    .await
    .map_err(|error| database_error("delete thread", error))?;
    let deleted = sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {} WHERE thread_id = $1",
        store.tables.threads
    )))
    .bind(params.thread_id.to_string())
    .execute(transaction.as_mut())
    .await
    .map_err(|error| database_error("delete thread", error))?;
    if deleted.rows_affected() != 1 {
        return Err(ThreadStoreError::ThreadNotFound {
            thread_id: params.thread_id,
        });
    }
    transaction
        .commit()
        .await
        .map_err(|error| database_error("delete thread", error))?;
    store.live_writers.lock().await.remove(&params.thread_id);
    Ok(())
}

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
    // Identity, fencing, and stream checks make an expired lease renewable only until another
    // writer takes over. The takeover path changes the identity and fencing token atomically.
    let renewed = sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET writer_lease_expires_at = CURRENT_TIMESTAMP + $1 * INTERVAL '1 millisecond' \
         WHERE thread_id = $2 AND writer_id = $3 AND fencing_token = $4 AND stream_version = $5",
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
    // Releasing an already-expired lease is a safe no-op while this writer's fence still matches.
    let released = sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET writer_lease_expires_at = CURRENT_TIMESTAMP \
         WHERE thread_id = $1 AND writer_id = $2 AND fencing_token = $3 AND stream_version = $4",
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
        let canonical_thread_exists: bool = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT EXISTS(SELECT 1 FROM {} WHERE thread_id = $1)",
            store.tables.threads
        )))
        .bind(thread_id.to_string())
        .fetch_one(&store.pool)
        .await
        .map_err(|error| database_error("shutdown thread history", error))?;
        if canonical_thread_exists {
            return Err(writer_conflict(thread_id));
        }
    }
    store.live_writers.lock().await.remove(&thread_id);
    Ok(())
}

pub(super) fn lease_millis() -> ThreadStoreResult<i64> {
    i64::try_from(WRITER_LEASE_DURATION.as_millis()).map_err(|_| ThreadStoreError::Internal {
        message: "thread writer lease duration is out of range".to_string(),
    })
}
