use super::super::LOG_PARTITION_ROW_LIMIT;
use super::super::LOG_PARTITION_SIZE_LIMIT_BYTES;
use super::LOG_RETENTION_DAYS;
use super::estimated_log_bytes;
use super::format_feedback_log_line;
use crate::LogEntry;
use crate::LogQuery;
use crate::LogRow;
use crate::postgres::map_sql_error;
use crate::postgres::qualified_table;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::QueryBuilder;
use std::collections::BTreeSet;

#[derive(Clone)]
pub(super) struct PostgresLogStore {
    pool: PgPool,
    schema: String,
    table: String,
}

impl PostgresLogStore {
    pub(super) fn new(pool: PgPool, schema: String) -> Self {
        let table = qualified_table(&schema, "logs");
        Self {
            pool,
            schema,
            table,
        }
    }

    pub(super) async fn insert_log(&self, entry: &LogEntry) -> anyhow::Result<()> {
        self.insert_logs(std::slice::from_ref(entry)).await
    }

    pub(super) async fn insert_logs(&self, entries: &[LogEntry]) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| map_sql_error(&self.schema, "begin log insert", error))?;
        let partition_keys = entries
            .iter()
            .map(|entry| match (&entry.thread_id, &entry.process_uuid) {
                (Some(thread_id), _) => format!("thread:{thread_id}"),
                (None, Some(process_uuid)) => format!("process:{process_uuid}"),
                (None, None) => "process:<null>".to_string(),
            })
            .collect::<BTreeSet<_>>();
        for partition_key in partition_keys {
            sqlx::query(
                "SELECT pg_advisory_xact_lock(hashtextextended(\
                 'codex-runtime-state:logs:' || $1, 0))",
            )
            .bind(format!("{}:{partition_key}", self.schema))
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_sql_error(&self.schema, "lock log partition", error))?;
        }
        let thread_ids = entries
            .iter()
            .filter_map(|entry| entry.thread_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let canonical_thread_ids = if thread_ids.is_empty() {
            BTreeSet::new()
        } else {
            let threads = qualified_table(&self.schema, "threads");
            sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
                "SELECT thread_id FROM {threads} WHERE thread_id = ANY($1) \
                     ORDER BY thread_id FOR KEY SHARE"
            )))
            .bind(&thread_ids)
            .fetch_all(&mut *transaction)
            .await
            .map_err(|error| {
                map_sql_error(&self.schema, "lock canonical threads for log insert", error)
            })?
            .into_iter()
            .collect()
        };
        let entries_to_insert = entries
            .iter()
            .filter(|entry| {
                entry
                    .thread_id
                    .as_ref()
                    .is_none_or(|thread_id| canonical_thread_ids.contains(thread_id))
            })
            .cloned()
            .collect::<Vec<_>>();
        anyhow::ensure!(
            !entries_to_insert.is_empty(),
            "Runtime State could not complete the `insert thread logs` operation; verify canonical thread state, then retry"
        );
        let mut builder = QueryBuilder::<Postgres>::new(format!(
            "INSERT INTO {} (ts, ts_nanos, level, target, feedback_log_body, thread_id, process_uuid, module_path, file, line, estimated_bytes) ",
            self.table
        ));
        builder.push_values(&entries_to_insert, |mut row, entry| {
            let feedback_log_body = entry.feedback_log_body.as_ref().or(entry.message.as_ref());
            row.push_bind(entry.ts)
                .push_bind(entry.ts_nanos)
                .push_bind(&entry.level)
                .push_bind(&entry.target)
                .push_bind(feedback_log_body)
                .push_bind(&entry.thread_id)
                .push_bind(&entry.process_uuid)
                .push_bind(&entry.module_path)
                .push_bind(&entry.file)
                .push_bind(entry.line)
                .push_bind(estimated_log_bytes(entry));
        });
        builder
            .build()
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_sql_error(&self.schema, "insert logs", error))?;
        self.prune_partitions(&entries_to_insert, &mut transaction)
            .await?;
        transaction
            .commit()
            .await
            .map_err(|error| map_sql_error(&self.schema, "commit log insert", error))?;
        Ok(())
    }

    async fn prune_partitions(
        &self,
        entries: &[LogEntry],
        transaction: &mut sqlx::Transaction<'_, Postgres>,
    ) -> anyhow::Result<()> {
        let thread_ids = entries
            .iter()
            .filter_map(|entry| entry.thread_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let process_uuids = entries
            .iter()
            .filter(|entry| entry.thread_id.is_none())
            .filter_map(|entry| entry.process_uuid.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let has_null_process = entries
            .iter()
            .any(|entry| entry.thread_id.is_none() && entry.process_uuid.is_none());

        let mut builder = QueryBuilder::<Postgres>::new(format!(
            "DELETE FROM {} WHERE id IN (SELECT id FROM (SELECT id, \
             SUM(estimated_bytes) OVER (PARTITION BY (thread_id IS NULL), \
             CASE WHEN thread_id IS NULL THEN process_uuid ELSE thread_id END \
             ORDER BY ts DESC, ts_nanos DESC, id DESC) AS cumulative_bytes, \
             ROW_NUMBER() OVER (PARTITION BY (thread_id IS NULL), \
             CASE WHEN thread_id IS NULL THEN process_uuid ELSE thread_id END \
             ORDER BY ts DESC, ts_nanos DESC, id DESC) AS row_number \
             FROM {} WHERE ",
            self.table, self.table
        ));
        let mut has_filter = false;
        if !thread_ids.is_empty() {
            builder
                .push("thread_id = ANY(")
                .push_bind(thread_ids)
                .push(")");
            has_filter = true;
        }
        if !process_uuids.is_empty() {
            if has_filter {
                builder.push(" OR ");
            }
            builder
                .push("(thread_id IS NULL AND process_uuid = ANY(")
                .push_bind(process_uuids)
                .push("))");
            has_filter = true;
        }
        if has_null_process {
            if has_filter {
                builder.push(" OR ");
            }
            builder.push("(thread_id IS NULL AND process_uuid IS NULL)");
        }
        builder
            .push(") AS ranked WHERE cumulative_bytes > ")
            .push_bind(LOG_PARTITION_SIZE_LIMIT_BYTES)
            .push(" OR row_number > ")
            .push_bind(LOG_PARTITION_ROW_LIMIT)
            .push(")");
        builder
            .build()
            .execute(&mut **transaction)
            .await
            .map_err(|error| map_sql_error(&self.schema, "prune log partitions", error))?;
        Ok(())
    }

    pub(super) async fn query_logs(&self, query: &LogQuery) -> anyhow::Result<Vec<LogRow>> {
        let mut builder = QueryBuilder::<Postgres>::new(format!(
            "SELECT id, ts, ts_nanos, level, target, feedback_log_body AS message, thread_id, process_uuid, file, line FROM {} WHERE 1 = 1",
            self.table
        ));
        push_log_filters(&mut builder, query);
        if query.descending {
            builder.push(" ORDER BY id DESC");
        } else {
            builder.push(" ORDER BY id ASC");
        }
        if let Some(limit) = query.limit {
            builder.push(" LIMIT ").push_bind(limit as i64);
        }
        builder
            .build_query_as::<LogRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(|error| map_sql_error(&self.schema, "query logs", error))
    }

    pub(super) async fn run_startup_maintenance(&self) -> anyhow::Result<()> {
        let Some(cutoff) =
            chrono::Utc::now().checked_sub_signed(chrono::Duration::days(LOG_RETENTION_DAYS))
        else {
            return Ok(());
        };
        let mut builder = QueryBuilder::<Postgres>::new(format!("DELETE FROM {}", self.table));
        builder
            .push(" WHERE ts < ")
            .push_bind(cutoff.timestamp())
            .build()
            .execute(&self.pool)
            .await
            .map_err(|error| map_sql_error(&self.schema, "delete expired logs", error))?;
        Ok(())
    }

    pub(super) async fn query_feedback_logs_for_threads(
        &self,
        thread_ids: &[&str],
    ) -> anyhow::Result<Vec<u8>> {
        if thread_ids.is_empty() {
            return Ok(Vec::new());
        }

        let requested_threads = thread_ids
            .iter()
            .map(|thread_id| (*thread_id).to_string())
            .collect::<Vec<_>>();
        let mut builder =
            QueryBuilder::<Postgres>::new("WITH requested_threads(thread_id) AS (SELECT unnest(");
        builder.push_bind(requested_threads).push(
            r#"::text[])),
latest_processes AS (
    SELECT (
        SELECT process_uuid
        FROM "#,
        );
        builder.push(&self.table).push(
            r#" AS logs
        WHERE logs.thread_id = requested_threads.thread_id AND process_uuid IS NOT NULL
        ORDER BY ts DESC, ts_nanos DESC, id DESC
        LIMIT 1
    ) AS process_uuid
    FROM requested_threads
),
feedback_logs AS (
    SELECT ts, ts_nanos, level, feedback_log_body, estimated_bytes, id
    FROM "#,
        );
        builder.push(&self.table).push(
            r#"
    WHERE feedback_log_body IS NOT NULL AND (
        thread_id IN (SELECT thread_id FROM requested_threads)
        OR (
            thread_id IS NULL
            AND process_uuid IN (
                SELECT process_uuid FROM latest_processes WHERE process_uuid IS NOT NULL
            )
        )
    )
),
bounded_feedback_logs AS (
    SELECT ts, ts_nanos, level, feedback_log_body, id,
        SUM(estimated_bytes) OVER (
            ORDER BY ts DESC, ts_nanos DESC, id DESC
        ) AS cumulative_estimated_bytes
    FROM feedback_logs
)
SELECT ts, ts_nanos, level, feedback_log_body
FROM bounded_feedback_logs
WHERE cumulative_estimated_bytes <= "#,
        );
        builder
            .push_bind(LOG_PARTITION_SIZE_LIMIT_BYTES)
            .push(" ORDER BY ts DESC, ts_nanos DESC, id DESC");
        let rows = builder
            .build_query_as::<FeedbackLogRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(|error| map_sql_error(&self.schema, "query feedback logs", error))?;

        let max_bytes = usize::try_from(LOG_PARTITION_SIZE_LIMIT_BYTES).unwrap_or(usize::MAX);
        let mut lines = Vec::new();
        let mut total_bytes = 0usize;
        for row in rows {
            let line =
                format_feedback_log_line(row.ts, row.ts_nanos, &row.level, &row.feedback_log_body);
            if total_bytes.saturating_add(line.len()) > max_bytes {
                break;
            }
            total_bytes += line.len();
            lines.push(line);
        }
        let mut ordered_bytes = Vec::with_capacity(total_bytes);
        for line in lines.into_iter().rev() {
            ordered_bytes.extend_from_slice(line.as_bytes());
        }
        Ok(ordered_bytes)
    }

    pub(super) async fn max_log_id(&self, query: &LogQuery) -> anyhow::Result<i64> {
        let mut builder = QueryBuilder::<Postgres>::new(format!(
            "SELECT MAX(id) AS max_id FROM {} WHERE 1 = 1",
            self.table
        ));
        push_log_filters(&mut builder, query);
        let max_id: Option<i64> = builder
            .build_query_scalar()
            .fetch_one(&self.pool)
            .await
            .map_err(|error| map_sql_error(&self.schema, "query maximum log ID", error))?;
        Ok(max_id.unwrap_or(0))
    }

    pub(super) async fn delete_logs_for_thread(&self, thread_id: &str) -> anyhow::Result<()> {
        let mut builder = QueryBuilder::<Postgres>::new(format!("DELETE FROM {}", self.table));
        builder
            .push(" WHERE thread_id = ")
            .push_bind(thread_id)
            .build()
            .execute(&self.pool)
            .await
            .map_err(|error| map_sql_error(&self.schema, "delete thread logs", error))?;
        Ok(())
    }

    pub(super) async fn close(&self) {
        self.pool.close().await;
    }
}

#[derive(sqlx::FromRow)]
struct FeedbackLogRow {
    ts: i64,
    ts_nanos: i64,
    level: String,
    feedback_log_body: String,
}

fn push_log_filters(builder: &mut QueryBuilder<Postgres>, query: &LogQuery) {
    if !query.levels_upper.is_empty() {
        builder.push(" AND UPPER(level) IN (");
        {
            let mut separated = builder.separated(", ");
            for level_upper in &query.levels_upper {
                separated.push_bind(level_upper.as_str());
            }
        }
        builder.push(")");
    }
    if let Some(from_ts) = query.from_ts {
        builder.push(" AND ts >= ").push_bind(from_ts);
    }
    if let Some(to_ts) = query.to_ts {
        builder.push(" AND ts <= ").push_bind(to_ts);
    }
    push_like_filters(builder, "module_path", &query.module_like);
    push_like_filters(builder, "file", &query.file_like);
    if !query.thread_ids.is_empty() || query.include_threadless {
        builder.push(" AND (");
        let mut needs_or = false;
        for thread_id in &query.thread_ids {
            if needs_or {
                builder.push(" OR ");
            }
            builder.push("thread_id = ").push_bind(thread_id.as_str());
            needs_or = true;
        }
        if query.include_threadless {
            if needs_or {
                builder.push(" OR ");
            }
            builder.push("thread_id IS NULL");
        }
        builder.push(")");
    }
    if let Some(after_id) = query.after_id {
        builder.push(" AND id > ").push_bind(after_id);
    }
    if let Some(search) = query.search.as_ref() {
        builder.push(" AND POSITION(");
        builder.push_bind(search.as_str());
        builder.push(" IN COALESCE(feedback_log_body, '')) > 0");
    }
}

fn push_like_filters(builder: &mut QueryBuilder<Postgres>, column: &str, filters: &[String]) {
    if filters.is_empty() {
        return;
    }
    builder.push(" AND (");
    for (index, filter) in filters.iter().enumerate() {
        if index > 0 {
            builder.push(" OR ");
        }
        builder
            .push(column)
            .push(" ILIKE '%' || ")
            .push_bind(filter.as_str())
            .push(" || '%'");
    }
    builder.push(")");
}
