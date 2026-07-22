use crate::postgres::qualified_table;
use crate::postgres::test_support::PostgresContractFixture;
use anyhow::Context;
use sqlx::AssertSqlSafe;
use std::path::Path;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct SourceSnapshot {
    entries: Vec<SourceEntry>,
}

#[derive(Debug, Eq, PartialEq)]
struct SourceEntry {
    relative_path: PathBuf,
    kind: SourceEntryKind,
    modified_nanos: u128,
    readonly: bool,
}

#[derive(Debug, Eq, PartialEq)]
enum SourceEntryKind {
    Directory,
    File(Vec<u8>),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct DestinationSnapshot {
    version: i64,
    schema_definition: Vec<String>,
    table_records: Vec<(String, String)>,
}

pub(super) fn snapshot_source(source_home: &Path) -> anyhow::Result<SourceSnapshot> {
    let mut pending = vec![source_home.to_path_buf()];
    let mut entries = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("read source snapshot directory {}", directory.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let relative_path = path
                .strip_prefix(source_home)
                .context("source snapshot entry escaped source home")?
                .to_path_buf();
            let metadata = std::fs::symlink_metadata(&path)?;
            let kind = if metadata.is_dir() {
                pending.push(path);
                SourceEntryKind::Directory
            } else {
                anyhow::ensure!(
                    metadata.is_file(),
                    "unexpected source entry: {}",
                    path.display()
                );
                SourceEntryKind::File(std::fs::read(&path)?)
            };
            let modified_nanos = metadata
                .modified()?
                .duration_since(UNIX_EPOCH)
                .context("source entry modification time predates Unix epoch")?
                .as_nanos();
            entries.push(SourceEntry {
                relative_path,
                kind,
                modified_nanos,
                readonly: metadata.permissions().readonly(),
            });
        }
    }
    entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(SourceSnapshot { entries })
}

pub(super) async fn snapshot_destination(
    destination: &PostgresContractFixture,
) -> anyhow::Result<DestinationSnapshot> {
    let pool = destination.connect_pool().await?;
    let result = async {
        let schema = destination.schema();
        let migration_table = qualified_table(schema, "_codex_runtime_state_migrations");
        let version = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT COALESCE(MAX(version), 0) FROM {migration_table}"
        )))
        .fetch_one(&pool)
        .await?;
        let mut schema_definition = sqlx::query_scalar::<_, String>(
            "SELECT concat_ws('|', c.relkind::text, c.relname, \
             COALESCE(pg_get_expr(c.relpartbound, c.oid), '')) \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 ORDER BY c.relkind, c.relname",
        )
        .bind(schema)
        .fetch_all(&pool)
        .await?;
        schema_definition.extend(
            sqlx::query_scalar::<_, String>(
                "SELECT concat_ws('|', table_name, ordinal_position::text, column_name, \
                 data_type, is_nullable, COALESCE(column_default, '')) \
                 FROM information_schema.columns WHERE table_schema = $1 \
                 ORDER BY table_name, ordinal_position",
            )
            .bind(schema)
            .fetch_all(&pool)
            .await?,
        );
        schema_definition.extend(
            sqlx::query_scalar::<_, String>(
                "SELECT concat_ws('|', c.conname, pg_get_constraintdef(c.oid)) \
                 FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace \
                 WHERE n.nspname = $1 ORDER BY c.conname",
            )
            .bind(schema)
            .fetch_all(&pool)
            .await?,
        );
        schema_definition.extend(
            sqlx::query_scalar::<_, String>(
                "SELECT concat_ws('|', indexname, indexdef) FROM pg_indexes \
                 WHERE schemaname = $1 ORDER BY indexname",
            )
            .bind(schema)
            .fetch_all(&pool)
            .await?,
        );
        let tables = sqlx::query_scalar::<_, String>(
            "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') ORDER BY c.relname",
        )
        .bind(schema)
        .fetch_all(&pool)
        .await?;
        let mut table_records = Vec::with_capacity(tables.len());
        for table in tables {
            let qualified = qualified_table(schema, &table);
            let records = sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT COALESCE(jsonb_agg(record ORDER BY record::text), '[]'::jsonb)::text \
                 FROM (SELECT to_jsonb(value) AS record FROM {qualified} value) records"
            )))
            .fetch_one(&pool)
            .await?;
            table_records.push((table, records));
        }
        anyhow::Ok(DestinationSnapshot {
            version,
            schema_definition,
            table_records,
        })
    }
    .await;
    pool.close().await;
    result
}
