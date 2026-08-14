use super::RuntimeStateThreadSnapshot;
use super::import_threads::thread_projection;
use crate::postgres::qualified_table;
use anyhow::Context;
use chrono::DateTime;
use codex_history::RolloutItem;
use futures::TryStreamExt;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use sqlx::AssertSqlSafe;
use sqlx::Row;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct ThreadContentEvidence {
    pub(super) threads_hash: String,
    pub(super) history_hash: String,
    pub(super) coordination_hash: String,
}

pub(super) fn snapshot_content_evidence(
    snapshot: &RuntimeStateThreadSnapshot,
) -> anyhow::Result<ThreadContentEvidence> {
    let mut threads_hasher = Sha256::new();
    let mut history_hasher = Sha256::new();
    let mut coordination_hasher = Sha256::new();
    for thread in &snapshot.threads {
        let session_meta = thread
            .canonical_history
            .lines()
            .iter()
            .find_map(|line| match &line.item {
                RolloutItem::SessionMeta(meta) => Some(meta),
                _ => None,
            })
            .context("Canonical Thread History has no SessionMeta")?;
        hash_field(
            &mut threads_hasher,
            thread.metadata.id.to_string().as_bytes(),
        );
        hash_json(
            &mut threads_hasher,
            &thread_projection(thread, session_meta)?,
        );
        hash_i64(
            &mut threads_hasher,
            i64::try_from(thread.canonical_history.lines().len())?,
        );
        hash_optional_i64(
            &mut threads_hasher,
            session_meta
                .meta
                .subagent_history_start_ordinal
                .map(i64::try_from)
                .transpose()?,
        );
        for timestamp in [
            Some(thread.metadata.created_at),
            Some(thread.metadata.updated_at),
            Some(thread.metadata.recency_at),
            thread.metadata.archived_at,
        ] {
            hash_optional_i64(
                &mut threads_hasher,
                timestamp.map(|value| value.timestamp_micros()),
            );
        }
        for (ordinal, line) in thread.canonical_history.lines().iter().enumerate() {
            hash_field(
                &mut history_hasher,
                thread.metadata.id.to_string().as_bytes(),
            );
            hash_i64(&mut history_hasher, i64::try_from(ordinal)?);
            hash_optional_i64(
                &mut history_hasher,
                line.ordinal.map(i64::try_from).transpose()?,
            );
            hash_json(&mut history_hasher, &serde_json::to_value(&line.item)?);
            hash_i64(
                &mut history_hasher,
                DateTime::parse_from_rfc3339(&line.timestamp)?.timestamp_micros(),
            );
        }
        if let Some(stream_version) = thread.polluted_at_stream_version {
            hash_field(
                &mut coordination_hasher,
                thread.metadata.id.to_string().as_bytes(),
            );
            hash_i64(&mut coordination_hasher, stream_version);
        }
    }
    for edge in &snapshot.spawn_edges {
        hash_field(
            &mut coordination_hasher,
            edge.parent_thread_id.to_string().as_bytes(),
        );
        hash_field(
            &mut coordination_hasher,
            edge.child_thread_id.to_string().as_bytes(),
        );
        hash_field(&mut coordination_hasher, edge.status.as_ref().as_bytes());
    }
    let backfill = &snapshot.backfill;
    hash_field(
        &mut coordination_hasher,
        backfill.state.status.as_str().as_bytes(),
    );
    hash_optional_string(
        &mut coordination_hasher,
        backfill.state.last_watermark.as_deref(),
    );
    for timestamp in [
        backfill.state.last_success_at,
        Some(backfill.updated_at),
        backfill.lease_expires_at,
    ] {
        hash_optional_i64(
            &mut coordination_hasher,
            timestamp.map(|value| value.timestamp_micros()),
        );
    }
    hash_optional_string(&mut coordination_hasher, backfill.owner_id.as_deref());
    hash_i64(&mut coordination_hasher, backfill.fencing_token);
    Ok(ThreadContentEvidence {
        threads_hash: format!("{:x}", threads_hasher.finalize()),
        history_hash: format!("{:x}", history_hasher.finalize()),
        coordination_hash: format!("{:x}", coordination_hasher.finalize()),
    })
}

pub(super) async fn thread_content_evidence(
    connection: &mut sqlx::PgConnection,
    schema: &str,
) -> anyhow::Result<ThreadContentEvidence> {
    let threads = qualified_table(schema, "threads");
    let mut threads_hasher = Sha256::new();
    let mut rows = sqlx::query(AssertSqlSafe(format!(
        "SELECT thread_id, projection, stream_version, history_projection_start_ordinal, \
         created_at, updated_at, recency_at, archived_at FROM {threads} ORDER BY thread_id COLLATE \"C\""
    )))
    .fetch(&mut *connection);
    while let Some(row) = rows.try_next().await? {
        hash_field(
            &mut threads_hasher,
            row.try_get::<String, _>("thread_id")?.as_bytes(),
        );
        hash_json(&mut threads_hasher, &row.try_get::<Value, _>("projection")?);
        hash_i64(&mut threads_hasher, row.try_get("stream_version")?);
        hash_optional_i64(
            &mut threads_hasher,
            row.try_get("history_projection_start_ordinal")?,
        );
        for timestamp in ["created_at", "updated_at", "recency_at", "archived_at"] {
            let value: Option<DateTime<chrono::Utc>> = row.try_get(timestamp)?;
            hash_optional_i64(
                &mut threads_hasher,
                value.map(|value| value.timestamp_micros()),
            );
        }
    }
    drop(rows);

    let history = qualified_table(schema, "thread_history");
    let mut history_hasher = Sha256::new();
    let mut rows = sqlx::query(AssertSqlSafe(format!(
        "SELECT thread_id, ordinal, source_ordinal, item, recorded_at FROM {history} \
         ORDER BY thread_id COLLATE \"C\", ordinal"
    )))
    .fetch(&mut *connection);
    while let Some(row) = rows.try_next().await? {
        hash_field(
            &mut history_hasher,
            row.try_get::<String, _>("thread_id")?.as_bytes(),
        );
        hash_i64(&mut history_hasher, row.try_get("ordinal")?);
        hash_optional_i64(&mut history_hasher, row.try_get("source_ordinal")?);
        hash_json(&mut history_hasher, &row.try_get::<Value, _>("item")?);
        hash_i64(
            &mut history_hasher,
            row.try_get::<DateTime<chrono::Utc>, _>("recorded_at")?
                .timestamp_micros(),
        );
    }
    drop(rows);

    let mut coordination_hasher = Sha256::new();
    let overrides = qualified_table(schema, "memory_thread_mode_overrides");
    let rows: Vec<(String, i64)> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT thread_id, polluted_at_stream_version FROM {overrides} \
         ORDER BY thread_id COLLATE \"C\""
    )))
    .fetch_all(&mut *connection)
    .await?;
    for (thread_id, stream_version) in rows {
        hash_field(&mut coordination_hasher, thread_id.as_bytes());
        hash_i64(&mut coordination_hasher, stream_version);
    }
    let edges = qualified_table(schema, "thread_spawn_edges");
    let rows: Vec<(String, String, String)> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT parent_thread_id, child_thread_id, status FROM {edges} \
         ORDER BY parent_thread_id COLLATE \"C\", child_thread_id COLLATE \"C\""
    )))
    .fetch_all(&mut *connection)
    .await?;
    for (parent_thread_id, child_thread_id, status) in rows {
        hash_field(&mut coordination_hasher, parent_thread_id.as_bytes());
        hash_field(&mut coordination_hasher, child_thread_id.as_bytes());
        hash_field(&mut coordination_hasher, status.as_bytes());
    }
    let backfill = qualified_table(schema, "backfill_state");
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT status, last_watermark, last_success_at, updated_at, owner_id, fencing_token, \
         lease_expires_at FROM {backfill} WHERE id = 1"
    )))
    .fetch_one(&mut *connection)
    .await?;
    hash_field(
        &mut coordination_hasher,
        row.try_get::<String, _>("status")?.as_bytes(),
    );
    hash_optional_string(
        &mut coordination_hasher,
        row.try_get::<Option<String>, _>("last_watermark")?
            .as_deref(),
    );
    for timestamp in ["last_success_at", "updated_at", "lease_expires_at"] {
        let value: Option<DateTime<chrono::Utc>> = row.try_get(timestamp)?;
        hash_optional_i64(
            &mut coordination_hasher,
            value.map(|value| value.timestamp_micros()),
        );
    }
    hash_optional_string(
        &mut coordination_hasher,
        row.try_get::<Option<String>, _>("owner_id")?.as_deref(),
    );
    hash_i64(&mut coordination_hasher, row.try_get("fencing_token")?);
    Ok(ThreadContentEvidence {
        threads_hash: format!("{:x}", threads_hasher.finalize()),
        history_hash: format!("{:x}", history_hasher.finalize()),
        coordination_hash: format!("{:x}", coordination_hasher.finalize()),
    })
}

fn hash_i64(hasher: &mut Sha256, value: i64) {
    hash_field(hasher, &value.to_be_bytes());
}

fn hash_optional_i64(hasher: &mut Sha256, value: Option<i64>) {
    match value {
        Some(value) => {
            hash_field(hasher, b"some");
            hash_i64(hasher, value);
        }
        None => hash_field(hasher, b"none"),
    }
}

fn hash_optional_string(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hash_field(hasher, b"some");
            hash_field(hasher, value.as_bytes());
        }
        None => hash_field(hasher, b"none"),
    }
}

fn hash_json(hasher: &mut Sha256, value: &Value) {
    match value {
        Value::Null => hash_field(hasher, b"null"),
        Value::Bool(value) => {
            hash_field(hasher, b"bool");
            hash_field(hasher, if *value { b"true" } else { b"false" });
        }
        Value::Number(value) => {
            hash_field(hasher, b"number");
            hash_field(hasher, value.to_string().as_bytes());
        }
        Value::String(value) => {
            hash_field(hasher, b"string");
            hash_field(hasher, value.as_bytes());
        }
        Value::Array(values) => {
            hash_field(hasher, b"array");
            for value in values {
                hash_json(hasher, value);
            }
        }
        Value::Object(values) => {
            hash_field(hasher, b"object");
            let mut fields = values.iter().collect::<Vec<_>>();
            fields.sort_unstable_by_key(|(key, _)| *key);
            for (key, value) in fields {
                hash_field(hasher, key.as_bytes());
                hash_json(hasher, value);
            }
        }
    }
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}
