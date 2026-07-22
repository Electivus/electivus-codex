use crate::PostgresNamespaceAction;
use crate::PostgresNamespaceConfig;
use crate::SqliteConfig;
use crate::postgres::MAXIMUM_COMPATIBLE_SCHEMA_VERSION;
use crate::postgres::config::connect_pool;
use crate::postgres::config::connection_failed;
use crate::postgres::manage_postgres_namespace_with_connection;
use crate::postgres::map_sql_error;
use crate::postgres::qualified_table;
use crate::runtime_db_paths;
use anyhow::Context;
use sha2::Digest;
use sha2::Sha256;
use sqlx::AssertSqlSafe;
use sqlx::Connection;
use std::path::Path;
use std::path::PathBuf;
use tokio::io::AsyncReadExt;

const IMPORTED_RESOURCE_PREFIX: &str = "memories/extensions/external_agent_import/";

#[path = "migration/source_lock.rs"]
mod source_lock;
#[path = "migration/source_validation.rs"]
mod source_validation;

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceInventory {
    databases: Vec<SqliteDatabaseInventory>,
    rollout_files: Vec<SourceFileInventory>,
    memory_files: Vec<SourceFileInventory>,
    imported_resources: Vec<SourceFileInventory>,
    configuration: Option<SourceFileInventory>,
}

/// Complete read-only inventory produced before an offline Runtime State Migration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeStateMigrationInventory {
    databases: Vec<SqliteDatabaseInventory>,
    rollout_files: Vec<SourceFileInventory>,
    memory_files: Vec<SourceFileInventory>,
    imported_resources: Vec<SourceFileInventory>,
    destination_schema: String,
    destination_schema_version: i64,
}

impl RuntimeStateMigrationInventory {
    pub fn databases(&self) -> &[SqliteDatabaseInventory] {
        &self.databases
    }

    pub fn rollout_files(&self) -> &[SourceFileInventory] {
        &self.rollout_files
    }

    pub fn memory_files(&self) -> &[SourceFileInventory] {
        &self.memory_files
    }

    pub fn imported_resources(&self) -> &[SourceFileInventory] {
        &self.imported_resources
    }

    pub fn destination_schema(&self) -> &str {
        &self.destination_schema
    }

    pub fn destination_schema_version(&self) -> i64 {
        self.destination_schema_version
    }
}

/// Logical and physical inventory for one mandatory SQLite runtime database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SqliteDatabaseInventory {
    label: &'static str,
    file: SourceFileInventory,
    tables: Vec<SqliteTableInventory>,
}

impl SqliteDatabaseInventory {
    pub fn label(&self) -> &'static str {
        self.label
    }

    pub fn file(&self) -> &SourceFileInventory {
        &self.file
    }

    pub fn tables(&self) -> &[SqliteTableInventory] {
        &self.tables
    }
}

/// Row count for one SQLite table, ordered by table name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SqliteTableInventory {
    name: String,
    row_count: i64,
}

impl SqliteTableInventory {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn row_count(&self) -> i64 {
        self.row_count
    }
}

/// Content identity for one source file, relative to the SQLite home.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFileInventory {
    relative_path: PathBuf,
    size_bytes: u64,
    sha256: String,
}

impl SourceFileInventory {
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Inspect a quiescent SQLite source and an empty, current PostgreSQL destination.
///
/// The operation opens SQLite with `immutable=1` and PostgreSQL in a read-only transaction. It
/// never runs migrations, copies records, edits configuration, or checkpoints SQLite journals.
pub async fn preflight_runtime_state_migration(
    source: SqliteConfig,
    destination: PostgresNamespaceConfig,
) -> anyhow::Result<RuntimeStateMigrationInventory> {
    let (destination_schema, destination_schema_version) =
        inspect_postgres_destination(&destination).await?;
    let source_inventory = inspect_source(&source).await?;
    let verification_inventory = inspect_source(&source).await?;
    anyhow::ensure!(
        source_inventory == verification_inventory,
        "Runtime State Migration source changed during preflight; stop every process using this SQLite home and retry"
    );
    let SourceInventory {
        databases,
        rollout_files,
        memory_files,
        imported_resources,
        configuration: _,
    } = source_inventory;

    Ok(RuntimeStateMigrationInventory {
        databases,
        rollout_files,
        memory_files,
        imported_resources,
        destination_schema,
        destination_schema_version,
    })
}

async fn inspect_source(source: &SqliteConfig) -> anyhow::Result<SourceInventory> {
    let databases = inspect_sqlite_databases(source).await?;
    let rollout_files = collect_files(source.home(), &["sessions", "archived_sessions"], |path| {
        path.extension()
            .is_some_and(|extension| extension == "jsonl")
    })
    .await?;
    let memory_artifacts = collect_files(source.home(), &["memories"], |_| true).await?;
    let (imported_resources, memory_files) = memory_artifacts.into_iter().partition(|file| {
        file.relative_path
            .to_string_lossy()
            .starts_with(IMPORTED_RESOURCE_PREFIX)
    });
    source_validation::validate_rollout_files(source, &rollout_files).await?;
    let configuration = if tokio::fs::try_exists(source.home().join("config.toml")).await? {
        Some(inventory_file(source.home(), Path::new("config.toml")).await?)
    } else {
        None
    };
    Ok(SourceInventory {
        databases,
        rollout_files,
        memory_files,
        imported_resources,
        configuration,
    })
}

async fn inspect_postgres_destination(
    config: &PostgresNamespaceConfig,
) -> anyhow::Result<(String, i64)> {
    let pool = connect_pool(config).await?;
    let result = async {
        let mut connection = pool
            .acquire()
            .await
            .map_err(|_| connection_failed(config.url_env()))?;
        let mut transaction = connection
            .begin()
            .await
            .map_err(|error| map_sql_error(config.schema(), "begin migration preflight", error))?;
        sqlx::query("SET TRANSACTION READ ONLY")
            .execute(transaction.as_mut())
            .await
            .map_err(|error| {
                map_sql_error(config.schema(), "make migration preflight read-only", error)
            })?;
        let status = manage_postgres_namespace_with_connection(
            config,
            transaction.as_mut(),
            PostgresNamespaceAction::Validate,
        )
        .await?;
        anyhow::ensure!(
            status.version() == MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
            "PostgreSQL schema `{}` is at version {}, but Runtime State Migration requires the current version {}; run `codex state schema migrate` and retry",
            status.schema(),
            status.version(),
            MAXIMUM_COMPATIBLE_SCHEMA_VERSION
        );
        ensure_postgres_namespace_empty(transaction.as_mut(), status.schema()).await?;
        transaction.rollback().await.map_err(|error| {
            map_sql_error(config.schema(), "finish migration preflight", error)
        })?;
        Ok((status.schema().to_string(), status.version()))
    }
    .await;
    pool.close().await;
    result
}

async fn ensure_postgres_namespace_empty(
    connection: &mut sqlx::PgConnection,
    schema: &str,
) -> anyhow::Result<()> {
    let backfill_state = qualified_table(schema, "backfill_state");
    let backfill_is_baseline: bool = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) = 1 AND BOOL_AND(\
         id = 1 AND status = 'pending' AND last_watermark IS NULL \
         AND last_success_at IS NULL AND owner_id IS NULL \
         AND fencing_token = 0 AND lease_expires_at IS NULL) \
         FROM {backfill_state}"
    )))
    .fetch_one(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "inspect destination backfill state", error))?;
    let generation_state = qualified_table(schema, "memory_generation_state");
    let generation_is_baseline: bool = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) = 1 AND BOOL_AND(singleton AND active_generation_id IS NULL) \
         FROM {generation_state}"
    )))
    .fetch_one(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "inspect destination memory state", error))?;
    anyhow::ensure!(
        backfill_is_baseline && generation_is_baseline,
        "PostgreSQL Runtime State Namespace `{schema}` contains non-baseline coordination state; provision an empty migrated namespace and retry"
    );
    let tables = sqlx::query_scalar::<_, String>(
        "SELECT c.relname FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') \
         AND c.relname <> '_codex_runtime_state_migrations' \
         AND c.relname <> 'backfill_state' \
         AND c.relname <> 'memory_generation_state' \
         ORDER BY c.relname",
    )
    .bind(schema)
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "inventory destination tables", error))?;
    let mut populated = Vec::new();
    for table in tables {
        let qualified = qualified_table(schema, &table);
        let row_count: i64 =
            sqlx::query_scalar(AssertSqlSafe(format!("SELECT COUNT(*) FROM {qualified}")))
                .fetch_one(&mut *connection)
                .await
                .map_err(|error| map_sql_error(schema, "count destination records", error))?;
        if row_count > 0 {
            populated.push(format!("{table} ({row_count})"));
        }
    }
    anyhow::ensure!(
        populated.is_empty(),
        "PostgreSQL Runtime State Namespace `{schema}` is not empty: {}; provision an empty migrated namespace and retry",
        populated.join(", ")
    );
    Ok(())
}

async fn inspect_sqlite_databases(
    source: &SqliteConfig,
) -> anyhow::Result<Vec<SqliteDatabaseInventory>> {
    let mut databases = Vec::new();
    for database in runtime_db_paths(source.home()) {
        let relative_path = database
            .path
            .strip_prefix(source.home())
            .unwrap_or(database.path.as_path());
        anyhow::ensure!(
            tokio::fs::try_exists(&database.path).await?,
            "required SQLite {} is missing at {}; restore it from backup or point the migration at the complete SQLite home",
            database.label,
            database.path.display()
        );
        reject_active_sqlite_writer(database.label, &database.path)?;
        reject_sqlite_sidecars(database.label, &database.path).await?;
        let pool = source
            .open_immutable_pool(&database.path)
            .await
            .with_context(|| {
                format!(
                    "failed to open SQLite {} at {}; stop all Codex writers and verify the source database",
                    database.label,
                    database.path.display()
                )
            })?;
        let inspection = async {
            let integrity = sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
                .fetch_all(&pool)
                .await?;
            anyhow::ensure!(
                integrity.as_slice() == ["ok"],
                "SQLite {} is corrupt: {}; restore a healthy source backup before retrying",
                database.label,
                integrity.join("; ")
            );
            let table_names = sqlx::query_scalar::<_, String>(
                "SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name",
            )
            .fetch_all(&pool)
            .await?;
            let mut tables = Vec::with_capacity(table_names.len());
            for name in table_names {
                let quoted_name = format!("\"{}\"", name.replace('"', "\"\""));
                let count_query = format!("SELECT COUNT(*) FROM {quoted_name}");
                let row_count = sqlx::query_scalar::<_, i64>(AssertSqlSafe(count_query))
                    .fetch_one(&pool)
                    .await?;
                tables.push(SqliteTableInventory { name, row_count });
            }
            anyhow::Ok(tables)
        }
        .await;
        pool.close().await;
        let tables = inspection.with_context(|| {
            format!(
                "failed to inspect SQLite {} at {}; restore a healthy source backup before retrying",
                database.label,
                database.path.display()
            )
        })?;
        databases.push(SqliteDatabaseInventory {
            label: database.label,
            file: inventory_file(source.home(), relative_path).await?,
            tables,
        });
    }
    Ok(databases)
}

fn reject_active_sqlite_writer(label: &str, path: &Path) -> anyhow::Result<()> {
    if source_lock::writer_is_active(path)
        .with_context(|| format!("probe SQLite {label} for active writers"))?
    {
        anyhow::bail!(
            "active SQLite writer detected for {label} at {}; stop every Codex process using this SQLite home and retry",
            path.display()
        );
    }
    Ok(())
}

async fn reject_sqlite_sidecars(label: &str, path: &Path) -> anyhow::Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        if tokio::fs::try_exists(&sidecar).await? {
            anyhow::bail!(
                "SQLite {label} has an uncheckpointed sidecar `{}`; stop every Codex writer, recover or checkpoint it with trusted SQLite tooling, and retry",
                sidecar.display()
            );
        }
    }
    Ok(())
}

async fn collect_files(
    source_home: &Path,
    roots: &[&str],
    include: impl Fn(&Path) -> bool,
) -> anyhow::Result<Vec<SourceFileInventory>> {
    let mut pending = Vec::new();
    for root in roots {
        let path = source_home.join(root);
        if tokio::fs::try_exists(&path).await? {
            pending.push(path);
        }
    }
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let mut entries = tokio::fs::read_dir(&directory)
            .await
            .with_context(|| format!("read source directory {}", directory.display()))?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let metadata = tokio::fs::symlink_metadata(&path).await?;
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "Runtime State Migration source must not contain symlinks: {}",
                path.display()
            );
            if metadata.is_dir() {
                if path.file_name().is_none_or(|name| name != ".git") {
                    pending.push(path);
                }
            } else if metadata.is_file() && include(&path) {
                if path
                    .file_name()
                    .is_some_and(|name| name == "phase2_workspace_diff.md")
                {
                    continue;
                }
                let relative_path = path.strip_prefix(source_home).with_context(|| {
                    format!("source file is outside SQLite home: {}", path.display())
                })?;
                files.push(inventory_file(source_home, relative_path).await?);
            } else if !metadata.is_file() {
                anyhow::bail!(
                    "Runtime State Migration source entry must be a regular file or directory: {}",
                    path.display()
                );
            }
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

async fn inventory_file(
    source_home: &Path,
    relative_path: &Path,
) -> anyhow::Result<SourceFileInventory> {
    let path = source_home.join(relative_path);
    let metadata_before = tokio::fs::metadata(&path).await?;
    let mut file = tokio::fs::File::open(&path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut size_bytes = 0_u64;
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        size_bytes = size_bytes
            .checked_add(read as u64)
            .context("source file size exceeded u64")?;
        hasher.update(&buffer[..read]);
    }
    let metadata_after = file.metadata().await?;
    anyhow::ensure!(
        metadata_before.len() == size_bytes
            && metadata_after.len() == size_bytes
            && metadata_before.modified()? == metadata_after.modified()?,
        "Runtime State Migration source file changed while it was being inventoried: {}; stop every process using this SQLite home and retry",
        path.display()
    );
    Ok(SourceFileInventory {
        relative_path: relative_path.to_path_buf(),
        size_bytes,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

#[cfg(test)]
#[path = "migration_tests.rs"]
mod tests;
