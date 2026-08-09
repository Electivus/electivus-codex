//! SQLite spawn-graph relations and integral thread deletion.

use super::postgres;
use super::*;
use codex_protocol::protocol::SessionSource;

impl StateRuntime {
    /// Persist or replace the directional parent-child edge for a spawned thread.
    pub async fn upsert_thread_spawn_edge(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> anyhow::Result<()> {
        if let Some((pool, schema)) = self.postgres_connection() {
            return postgres::upsert_thread_spawn_edge(
                &pool,
                &schema,
                parent_thread_id,
                child_thread_id,
                status,
            )
            .await;
        }
        sqlx::query(
            r#"
INSERT INTO thread_spawn_edges (
    parent_thread_id,
    child_thread_id,
    status
) VALUES (?, ?, ?)
ON CONFLICT(child_thread_id) DO UPDATE SET
    parent_thread_id = excluded.parent_thread_id,
    status = excluded.status
            "#,
        )
        .bind(parent_thread_id.to_string())
        .bind(child_thread_id.to_string())
        .bind(status.as_ref())
        .execute(self.sqlite_pool()?)
        .await?;
        Ok(())
    }

    /// Update the persisted lifecycle status of a spawned thread's incoming edge.
    pub async fn set_thread_spawn_edge_status(
        &self,
        child_thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> anyhow::Result<()> {
        if let Some((pool, schema)) = self.postgres_connection() {
            return postgres::set_thread_spawn_edge_status(&pool, &schema, child_thread_id, status)
                .await;
        }
        sqlx::query("UPDATE thread_spawn_edges SET status = ? WHERE child_thread_id = ?")
            .bind(status.as_ref())
            .bind(child_thread_id.to_string())
            .execute(self.sqlite_pool()?)
            .await?;
        Ok(())
    }

    /// List direct spawned children of `parent_thread_id` whose edge matches `status`.
    pub async fn list_thread_spawn_children_with_status(
        &self,
        parent_thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> anyhow::Result<Vec<ThreadId>> {
        self.list_thread_spawn_children_matching(parent_thread_id, Some(status))
            .await
    }

    /// List all direct spawned children of `parent_thread_id`.
    pub async fn list_thread_spawn_children(
        &self,
        parent_thread_id: ThreadId,
    ) -> anyhow::Result<Vec<ThreadId>> {
        self.list_thread_spawn_children_matching(parent_thread_id, /*status*/ None)
            .await
    }

    /// List spawned descendants of `root_thread_id` whose edges match `status`.
    ///
    /// Descendants are returned breadth-first by depth, then by thread id for stable ordering.
    pub async fn list_thread_spawn_descendants_with_status(
        &self,
        root_thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> anyhow::Result<Vec<ThreadId>> {
        self.list_thread_spawn_descendants_matching(root_thread_id, Some(status))
            .await
    }

    /// List all spawned descendants of `root_thread_id`.
    ///
    /// Descendants are returned breadth-first by depth, then by thread id for stable ordering.
    pub async fn list_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
    ) -> anyhow::Result<Vec<ThreadId>> {
        self.list_thread_spawn_descendants_matching(root_thread_id, /*status*/ None)
            .await
    }

    /// Find a direct spawned child of `parent_thread_id` by canonical agent path.
    pub async fn find_thread_spawn_child_by_path(
        &self,
        parent_thread_id: ThreadId,
        agent_path: &str,
    ) -> anyhow::Result<Option<ThreadId>> {
        let rows = sqlx::query(
            r#"
SELECT threads.id
FROM thread_spawn_edges
JOIN threads ON threads.id = thread_spawn_edges.child_thread_id
WHERE thread_spawn_edges.parent_thread_id = ?
  AND threads.agent_path = ?
ORDER BY threads.id
LIMIT 2
            "#,
        )
        .bind(parent_thread_id.to_string())
        .bind(agent_path)
        .fetch_all(self.sqlite_pool()?)
        .await?;
        one_thread_id_from_rows(rows, agent_path)
    }

    /// Find a spawned descendant of `root_thread_id` by canonical agent path.
    pub async fn find_thread_spawn_descendant_by_path(
        &self,
        root_thread_id: ThreadId,
        agent_path: &str,
    ) -> anyhow::Result<Option<ThreadId>> {
        let rows = sqlx::query(
            r#"
WITH RECURSIVE subtree(child_thread_id) AS (
    SELECT child_thread_id
    FROM thread_spawn_edges
    WHERE parent_thread_id = ?
    UNION ALL
    SELECT edge.child_thread_id
    FROM thread_spawn_edges AS edge
    JOIN subtree ON edge.parent_thread_id = subtree.child_thread_id
)
SELECT threads.id
FROM subtree
JOIN threads ON threads.id = subtree.child_thread_id
WHERE threads.agent_path = ?
ORDER BY threads.id
LIMIT 2
            "#,
        )
        .bind(root_thread_id.to_string())
        .bind(agent_path)
        .fetch_all(self.sqlite_pool()?)
        .await?;
        one_thread_id_from_rows(rows, agent_path)
    }

    async fn list_thread_spawn_children_matching(
        &self,
        parent_thread_id: ThreadId,
        status: Option<crate::DirectionalThreadSpawnEdgeStatus>,
    ) -> anyhow::Result<Vec<ThreadId>> {
        if let Some((pool, schema)) = self.postgres_connection() {
            return postgres::list_thread_spawn_children(&pool, &schema, parent_thread_id, status)
                .await;
        }
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT child_thread_id FROM thread_spawn_edges WHERE parent_thread_id = ",
        );
        builder.push_bind(parent_thread_id.to_string());
        if let Some(status) = status {
            builder.push(" AND status = ").push_bind(status.to_string());
        }
        builder.push(" ORDER BY child_thread_id");

        let rows = builder.build().fetch_all(self.sqlite_pool()?).await?;
        rows.into_iter()
            .map(|row| {
                ThreadId::try_from(row.try_get::<String, _>("child_thread_id")?).map_err(Into::into)
            })
            .collect()
    }

    async fn list_thread_spawn_descendants_matching(
        &self,
        root_thread_id: ThreadId,
        status: Option<crate::DirectionalThreadSpawnEdgeStatus>,
    ) -> anyhow::Result<Vec<ThreadId>> {
        if let Some((pool, schema)) = self.postgres_connection() {
            return postgres::list_thread_spawn_descendants(&pool, &schema, root_thread_id, status)
                .await;
        }
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
WITH RECURSIVE subtree(child_thread_id, depth) AS (
    SELECT child_thread_id, 1
    FROM thread_spawn_edges
    WHERE parent_thread_id =
            "#,
        );
        builder.push_bind(root_thread_id.to_string());
        if let Some(status) = status {
            let status = status.to_string();
            builder.push(" AND status = ").push_bind(status.clone());
            builder.push(
                r#"
    UNION ALL
    SELECT edge.child_thread_id, subtree.depth + 1
    FROM thread_spawn_edges AS edge
    JOIN subtree ON edge.parent_thread_id = subtree.child_thread_id
    WHERE status =
                "#,
            );
            builder.push_bind(status);
        } else {
            builder.push(
                r#"
    UNION ALL
    SELECT edge.child_thread_id, subtree.depth + 1
    FROM thread_spawn_edges AS edge
    JOIN subtree ON edge.parent_thread_id = subtree.child_thread_id
                "#,
            );
        }
        builder.push(
            r#"
)
SELECT child_thread_id
FROM subtree
ORDER BY depth ASC, child_thread_id ASC
            "#,
        );

        let rows = builder.build().fetch_all(self.sqlite_pool()?).await?;
        rows.into_iter()
            .map(|row| {
                ThreadId::try_from(row.try_get::<String, _>("child_thread_id")?).map_err(Into::into)
            })
            .collect()
    }

    async fn insert_thread_spawn_edge_if_absent(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"
INSERT INTO thread_spawn_edges (
    parent_thread_id,
    child_thread_id,
    status
) VALUES (?, ?, ?)
ON CONFLICT(child_thread_id) DO NOTHING
            "#,
        )
        .bind(parent_thread_id.to_string())
        .bind(child_thread_id.to_string())
        .bind(crate::DirectionalThreadSpawnEdgeStatus::Open.as_ref())
        .execute(self.sqlite_pool()?)
        .await?;
        Ok(())
    }

    pub(super) async fn insert_thread_spawn_edge_from_source_if_absent(
        &self,
        child_thread_id: ThreadId,
        source: &str,
    ) -> anyhow::Result<()> {
        let Some(parent_thread_id) = thread_spawn_parent_thread_id_from_source_str(source) else {
            return Ok(());
        };
        self.insert_thread_spawn_edge_if_absent(parent_thread_id, child_thread_id)
            .await
    }

    /// Delete a thread and all associated state by id.
    pub async fn delete_thread(&self, thread_id: ThreadId) -> anyhow::Result<u64> {
        self.delete_threads_strict(&[thread_id]).await
    }

    /// Atomically discover and delete a thread plus its complete spawned subtree.
    ///
    /// PostgreSQL child creation shares canonical row locks with this operation, so a child that
    /// commits while deletion is waiting is included before the transaction can commit.
    pub async fn delete_thread_spawn_subtree_strict(
        &self,
        root_thread_id: ThreadId,
    ) -> anyhow::Result<Vec<ThreadId>> {
        if let Some((pool, schema)) = self.postgres_connection() {
            return postgres::delete_thread_spawn_subtree_strict(&pool, &schema, root_thread_id)
                .await;
        }

        let mut thread_ids = vec![root_thread_id];
        thread_ids.extend(self.list_thread_spawn_descendants(root_thread_id).await?);
        self.delete_threads_strict(&thread_ids).await?;
        Ok(thread_ids)
    }

    /// Delete a set of threads and all associated state.
    ///
    /// Spawn edges and thread rows are deleted last so a failed delete can be retried with enough
    /// state left to rediscover the same spawned subtree.
    pub async fn delete_threads_strict(&self, thread_ids: &[ThreadId]) -> anyhow::Result<u64> {
        if thread_ids.is_empty() {
            return Ok(0);
        }

        let mut thread_ids = thread_ids
            .iter()
            .map(|thread_id| (*thread_id, thread_id.to_string()))
            .collect::<Vec<_>>();
        thread_ids.sort_unstable_by(|(_, left), (_, right)| left.cmp(right));
        thread_ids.dedup_by(|(_, left), (_, right)| left == right);
        let thread_id_strings = thread_ids
            .iter()
            .map(|(_, thread_id)| thread_id.clone())
            .collect::<Vec<_>>();
        if let Some((pool, schema)) = self.postgres_connection() {
            return postgres::delete_threads_strict(&pool, &schema, &thread_id_strings).await;
        }

        for (thread_id, thread_id_string) in &thread_ids {
            self.logs.delete_logs_for_thread(thread_id_string).await?;
            if let Some(thread_queue) = self.thread_queue() {
                thread_queue.delete_thread_queue(*thread_id).await?;
            }
            self.memories.delete_thread_memory(*thread_id).await?;
            self.thread_goals.delete_thread_goal(*thread_id).await?;
        }

        let mut tx = self.sqlite_pool()?.begin().await?;
        for thread_id_string in &thread_id_strings {
            sqlx::query("DELETE FROM thread_dynamic_tools WHERE thread_id = ?")
                .bind(thread_id_string)
                .execute(&mut *tx)
                .await?;
        }
        for thread_id_string in &thread_id_strings {
            sqlx::query(
                "DELETE FROM thread_spawn_edges WHERE parent_thread_id = ? OR child_thread_id = ?",
            )
            .bind(thread_id_string)
            .bind(thread_id_string)
            .execute(&mut *tx)
            .await?;
        }
        let mut rows_affected = 0;
        for thread_id_string in &thread_id_strings {
            rows_affected += sqlx::query("DELETE FROM threads WHERE id = ?")
                .bind(thread_id_string)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;

        Ok(rows_affected)
    }
}

fn one_thread_id_from_rows(
    rows: Vec<sqlx::sqlite::SqliteRow>,
    agent_path: &str,
) -> anyhow::Result<Option<ThreadId>> {
    let mut ids = rows
        .into_iter()
        .map(|row| {
            let id: String = row.try_get("id")?;
            ThreadId::try_from(id).map_err(anyhow::Error::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    match ids.len() {
        0 => Ok(None),
        1 => Ok(ids.pop()),
        _ => Err(anyhow::anyhow!(
            "multiple agents found for canonical path `{agent_path}`"
        )),
    }
}

fn thread_spawn_parent_thread_id_from_source_str(source: &str) -> Option<ThreadId> {
    let parsed_source = serde_json::from_str(source)
        .or_else(|_| serde_json::from_value::<SessionSource>(Value::String(source.to_string())));
    parsed_source.ok()?.parent_thread_id()
}
