//! PostgreSQL thread metadata, relation, lookup, and deletion operations.

use super::ThreadResumeMetadata;
use crate::DirectionalThreadSpawnEdgeStatus;
use crate::postgres::map_sql_error;
use crate::postgres::qualified_table;
use codex_protocol::ThreadId;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::QueryBuilder;
use sqlx::Row;
use std::path::PathBuf;

pub(super) async fn get_thread_resume_metadata(
    pool: &PgPool,
    schema: &str,
    id: ThreadId,
) -> anyhow::Result<Option<ThreadResumeMetadata>> {
    let threads = qualified_table(schema, "threads");
    let projection: Option<Value> = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT projection FROM {threads} WHERE thread_id = $1"
    )))
    .bind(id.to_string())
    .fetch_optional(pool)
    .await
    .map_err(|error| map_sql_error(schema, "read thread resume metadata", error))?;
    projection
        .map(serde_json::from_value)
        .transpose()
        .map_err(Into::into)
}

pub(super) async fn mark_thread_paginated(
    pool: &PgPool,
    schema: &str,
    thread_id: ThreadId,
) -> anyhow::Result<bool> {
    let threads = qualified_table(schema, "threads");
    let result = sqlx::query(AssertSqlSafe(format!(
        "UPDATE {threads} SET projection = jsonb_set(projection, '{{history_mode}}', \
         to_jsonb('paginated'::text), TRUE) WHERE thread_id = $1 AND \
         COALESCE(projection ->> 'history_mode', 'legacy') <> 'paginated'"
    )))
    .bind(thread_id.to_string())
    .execute(pool)
    .await
    .map_err(|error| map_sql_error(schema, "promote thread history mode", error))?;
    Ok(result.rows_affected() > 0)
}

pub(super) async fn set_thread_preview_if_empty(
    pool: &PgPool,
    schema: &str,
    thread_id: ThreadId,
    preview: &str,
) -> anyhow::Result<bool> {
    let threads = qualified_table(schema, "threads");
    let result = sqlx::query(AssertSqlSafe(format!(
        "UPDATE {threads} \
         SET projection = jsonb_set(projection, '{{preview}}', to_jsonb($1::text), TRUE) \
         WHERE thread_id = $2 AND COALESCE(projection ->> 'preview', '') = ''"
    )))
    .bind(preview)
    .bind(thread_id.to_string())
    .execute(pool)
    .await
    .map_err(|error| map_sql_error(schema, "set empty thread preview", error))?;
    Ok(result.rows_affected() > 0)
}

pub(super) async fn upsert_thread_spawn_edge(
    pool: &PgPool,
    schema: &str,
    parent_thread_id: ThreadId,
    child_thread_id: ThreadId,
    status: DirectionalThreadSpawnEdgeStatus,
) -> anyhow::Result<()> {
    let table = qualified_table(schema, "thread_spawn_edges");
    let threads = qualified_table(schema, "threads");
    let mut thread_ids = vec![parent_thread_id.to_string(), child_thread_id.to_string()];
    thread_ids.sort_unstable();
    thread_ids.dedup();
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin thread spawn edge upsert", error))?;
    let locked_thread_ids = sqlx::query_scalar::<_, String>(AssertSqlSafe(format!(
        "SELECT thread_id FROM {threads} WHERE thread_id = ANY($1) \
         ORDER BY thread_id FOR KEY SHARE"
    )))
    .bind(&thread_ids)
    .fetch_all(&mut *transaction)
    .await
    .map_err(|error| {
        map_sql_error(
            schema,
            "lock canonical threads for spawn edge upsert",
            error,
        )
    })?;
    anyhow::ensure!(
        locked_thread_ids.len() == thread_ids.len(),
        "Runtime State could not complete the `upsert thread spawn edge` operation; verify canonical thread state, then retry"
    );
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {table} (parent_thread_id, child_thread_id, status) \
         VALUES ($1, $2, $3) ON CONFLICT(child_thread_id) DO UPDATE SET \
         parent_thread_id = excluded.parent_thread_id, status = excluded.status"
    )))
    .bind(parent_thread_id.to_string())
    .bind(child_thread_id.to_string())
    .bind(status.as_ref())
    .execute(&mut *transaction)
    .await
    .map_err(|error| map_sql_error(schema, "upsert thread spawn edge", error))?;
    transaction
        .commit()
        .await
        .map_err(|error| map_sql_error(schema, "commit thread spawn edge upsert", error))
}

pub(super) async fn set_thread_spawn_edge_status(
    pool: &PgPool,
    schema: &str,
    child_thread_id: ThreadId,
    status: DirectionalThreadSpawnEdgeStatus,
) -> anyhow::Result<()> {
    let table = qualified_table(schema, "thread_spawn_edges");
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {table} SET status = $1 WHERE child_thread_id = $2"
    )))
    .bind(status.as_ref())
    .bind(child_thread_id.to_string())
    .execute(pool)
    .await
    .map_err(|error| map_sql_error(schema, "update thread spawn edge", error))?;
    Ok(())
}

pub(super) async fn list_thread_spawn_children(
    pool: &PgPool,
    schema: &str,
    parent_thread_id: ThreadId,
    status: Option<DirectionalThreadSpawnEdgeStatus>,
) -> anyhow::Result<Vec<ThreadId>> {
    let table = qualified_table(schema, "thread_spawn_edges");
    let rows = match status {
        Some(status) => {
            sqlx::query(AssertSqlSafe(format!(
                "SELECT child_thread_id FROM {table} WHERE parent_thread_id = $1 \
                 AND status = $2 ORDER BY child_thread_id"
            )))
            .bind(parent_thread_id.to_string())
            .bind(status.as_ref())
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query(AssertSqlSafe(format!(
                "SELECT child_thread_id FROM {table} WHERE parent_thread_id = $1 \
                 ORDER BY child_thread_id"
            )))
            .bind(parent_thread_id.to_string())
            .fetch_all(pool)
            .await
        }
    }
    .map_err(|error| map_sql_error(schema, "list thread spawn children", error))?;
    decode_thread_ids(rows, schema, "list thread spawn children")
}

pub(super) async fn list_thread_spawn_descendants(
    pool: &PgPool,
    schema: &str,
    root_thread_id: ThreadId,
    status: Option<DirectionalThreadSpawnEdgeStatus>,
) -> anyhow::Result<Vec<ThreadId>> {
    let table = qualified_table(schema, "thread_spawn_edges");
    let rows = match status {
        Some(status) => {
            sqlx::query(AssertSqlSafe(format!(
                "WITH RECURSIVE subtree(child_thread_id, depth) AS ( \
                 SELECT child_thread_id, 1 FROM {table} WHERE parent_thread_id = $1 \
                 AND status = $2 UNION ALL SELECT edge.child_thread_id, subtree.depth + 1 \
                 FROM {table} AS edge JOIN subtree ON edge.parent_thread_id = \
                 subtree.child_thread_id WHERE edge.status = $2) SELECT child_thread_id \
                 FROM subtree ORDER BY depth ASC, child_thread_id ASC"
            )))
            .bind(root_thread_id.to_string())
            .bind(status.as_ref())
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query(AssertSqlSafe(format!(
                "WITH RECURSIVE subtree(child_thread_id, depth) AS ( \
                 SELECT child_thread_id, 1 FROM {table} WHERE parent_thread_id = $1 \
                 UNION ALL SELECT edge.child_thread_id, subtree.depth + 1 FROM {table} \
                 AS edge JOIN subtree ON edge.parent_thread_id = subtree.child_thread_id) \
                 SELECT child_thread_id FROM subtree ORDER BY depth ASC, child_thread_id ASC"
            )))
            .bind(root_thread_id.to_string())
            .fetch_all(pool)
            .await
        }
    }
    .map_err(|error| map_sql_error(schema, "list thread spawn descendants", error))?;
    decode_thread_ids(rows, schema, "list thread spawn descendants")
}

pub(super) async fn find_rollout_path_by_id(
    pool: &PgPool,
    schema: &str,
    id: ThreadId,
    archived_only: Option<bool>,
) -> anyhow::Result<Option<PathBuf>> {
    let table = qualified_table(schema, "threads");
    let mut builder = QueryBuilder::<Postgres>::new(format!(
        "SELECT projection ->> 'rollout_path' AS rollout_path FROM {table} \
         WHERE thread_id = "
    ));
    builder.push_bind(id.to_string());
    match archived_only {
        Some(true) => {
            builder.push(" AND projection ->> 'archived_at' IS NOT NULL");
        }
        Some(false) => {
            builder.push(" AND projection ->> 'archived_at' IS NULL");
        }
        None => {}
    }
    let row = builder
        .build()
        .fetch_optional(pool)
        .await
        .map_err(|error| map_sql_error(schema, "find thread rollout path", error))?;
    row.map(|row| {
        row.try_get::<Option<String>, _>("rollout_path")
            .map(|path| path.map(PathBuf::from))
            .map_err(|error| map_sql_error(schema, "decode thread rollout path", error))
    })
    .transpose()
    .map(Option::flatten)
}

pub(super) async fn delete_thread_spawn_subtree_strict(
    pool: &PgPool,
    schema: &str,
    root_thread_id: ThreadId,
) -> anyhow::Result<Vec<ThreadId>> {
    let logs = qualified_table(schema, "logs");
    let threads = qualified_table(schema, "threads");
    let spawn_edges = qualified_table(schema, "thread_spawn_edges");
    let root_thread_id_string = root_thread_id.to_string();
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin thread subtree deletion", error))?;
    let thread_id_strings = loop {
        let discovered = discover_thread_spawn_subtree(
            &mut transaction,
            schema,
            &spawn_edges,
            &root_thread_id_string,
            "discover thread spawn subtree",
        )
        .await?;
        let mut lock_order = discovered.clone();
        lock_order.sort_unstable();
        sqlx::query_scalar::<_, String>(AssertSqlSafe(format!(
            "SELECT thread_id FROM {threads} WHERE thread_id = ANY($1) \
             ORDER BY thread_id FOR UPDATE"
        )))
        .bind(&lock_order)
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| map_sql_error(schema, "lock thread spawn subtree for deletion", error))?;
        let stable = discover_thread_spawn_subtree(
            &mut transaction,
            schema,
            &spawn_edges,
            &root_thread_id_string,
            "verify thread spawn subtree",
        )
        .await?;
        if stable == discovered {
            break stable;
        }
    };
    let deleted_thread_ids = thread_id_strings
        .iter()
        .map(|thread_id| ThreadId::try_from(thread_id.as_str()).map_err(Into::into))
        .collect::<anyhow::Result<Vec<_>>>()?;
    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {logs} WHERE thread_id = ANY($1)"
    )))
    .bind(&thread_id_strings)
    .execute(&mut *transaction)
    .await
    .map_err(|error| map_sql_error(schema, "delete thread subtree logs", error))?;
    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {threads} WHERE thread_id = ANY($1)"
    )))
    .bind(&thread_id_strings)
    .execute(&mut *transaction)
    .await
    .map_err(|error| map_sql_error(schema, "delete Runtime State thread subtree", error))?;
    transaction
        .commit()
        .await
        .map_err(|error| map_sql_error(schema, "commit thread subtree deletion", error))?;
    Ok(deleted_thread_ids)
}

pub(super) async fn delete_threads_strict(
    pool: &PgPool,
    schema: &str,
    thread_id_strings: &[String],
) -> anyhow::Result<u64> {
    let logs = qualified_table(schema, "logs");
    let threads = qualified_table(schema, "threads");
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin thread deletion", error))?;
    sqlx::query_scalar::<_, String>(AssertSqlSafe(format!(
        "SELECT thread_id FROM {threads} WHERE thread_id = ANY($1) \
         ORDER BY thread_id FOR UPDATE"
    )))
    .bind(thread_id_strings)
    .fetch_all(&mut *transaction)
    .await
    .map_err(|error| map_sql_error(schema, "lock threads for deletion", error))?;
    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {logs} WHERE thread_id = ANY($1)"
    )))
    .bind(thread_id_strings)
    .execute(&mut *transaction)
    .await
    .map_err(|error| map_sql_error(schema, "delete thread logs", error))?;
    let rows_affected = sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {threads} WHERE thread_id = ANY($1)"
    )))
    .bind(thread_id_strings)
    .execute(&mut *transaction)
    .await
    .map_err(|error| map_sql_error(schema, "delete Runtime State threads", error))?
    .rows_affected();
    transaction
        .commit()
        .await
        .map_err(|error| map_sql_error(schema, "commit thread deletion", error))?;
    Ok(rows_affected)
}

async fn discover_thread_spawn_subtree(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    schema: &str,
    spawn_edges: &str,
    root_thread_id: &str,
    operation: &'static str,
) -> anyhow::Result<Vec<String>> {
    sqlx::query_scalar::<_, String>(AssertSqlSafe(format!(
        "WITH RECURSIVE subtree(thread_id) AS (\
         SELECT $1::text UNION \
         SELECT edge.child_thread_id FROM {spawn_edges} AS edge \
         JOIN subtree ON edge.parent_thread_id = subtree.thread_id\
         ) SELECT thread_id FROM subtree \
         ORDER BY (thread_id <> $1), thread_id"
    )))
    .bind(root_thread_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(|error| map_sql_error(schema, operation, error))
}

fn decode_thread_ids(
    rows: Vec<sqlx::postgres::PgRow>,
    schema: &str,
    operation: &'static str,
) -> anyhow::Result<Vec<ThreadId>> {
    rows.into_iter()
        .map(|row| {
            let thread_id: String = row
                .try_get("child_thread_id")
                .map_err(|error| map_sql_error(schema, operation, error))?;
            ThreadId::try_from(thread_id).map_err(Into::into)
        })
        .collect()
}
