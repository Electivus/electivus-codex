use crate::open_thread_history_db;
use crate::postgres::qualified_table;
use crate::postgres::test_support::PostgresContractFixture;
use anyhow::Context;
use sqlx::AssertSqlSafe;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
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

pub(super) async fn initialized_source(prefix: &str) -> anyhow::Result<PathBuf> {
    let (source, runtime) = initialized_runtime_source(prefix).await?;
    runtime.close().await;
    Ok(source)
}

pub(super) async fn initialized_runtime_source(
    prefix: &str,
) -> anyhow::Result<(PathBuf, std::sync::Arc<crate::StateRuntime>)> {
    let source = std::env::temp_dir().join(format!(
        "codex-migration-{prefix}-{}-{}",
        std::process::id(),
        SystemTime::UNIX_EPOCH.elapsed()?.as_nanos()
    ));
    std::fs::create_dir(&source)?;
    let runtime =
        crate::StateRuntime::init_sqlite(source.clone(), "test-provider".to_string()).await?;
    let history = open_thread_history_db(runtime.sqlite()).await?;
    history.close().await;
    std::fs::write(source.join("config.toml"), b"model = \"gpt-5\"\n")?;
    Ok((source, runtime))
}

pub(super) async fn source_with_rollout(
    prefix: &str,
    rollout_path: impl FnOnce(&Path) -> PathBuf,
) -> anyhow::Result<PathBuf> {
    let (source, runtime) = initialized_runtime_source(prefix).await?;
    let mut metadata = crate::runtime::test_support::test_thread_metadata(
        &source,
        codex_protocol::ThreadId::new(),
        source.clone(),
    );
    metadata.rollout_path = rollout_path(&source);
    runtime.upsert_thread(&metadata).await?;
    runtime.close().await;
    Ok(source)
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
                "SELECT definition FROM (\
                 SELECT concat_ws('|', 'view', viewname, definition) AS definition FROM pg_views WHERE schemaname = $1 UNION ALL \
                 SELECT concat_ws('|', 'function', p.proname, pg_get_functiondef(p.oid)) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = $1 UNION ALL \
                 SELECT concat_ws('|', 'trigger', t.tgname, pg_get_triggerdef(t.oid)) FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 UNION ALL \
                 SELECT concat_ws('|', 'policy', tablename, policyname, cmd, qual, with_check) FROM pg_policies WHERE schemaname = $1 UNION ALL \
                 SELECT concat_ws('|', 'grant', table_name, grantee, privilege_type) FROM information_schema.table_privileges WHERE table_schema = $1 UNION ALL \
                 SELECT concat_ws('|', 'sequence', sequencename, start_value, min_value, max_value, increment_by, cycle, cache_size, last_value) FROM pg_sequences WHERE schemaname = $1) definitions ORDER BY definition",
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
