use crate::PostgresNamespaceConfig;
use crate::SqliteConfig;
use anyhow::Context;
use sha2::Digest;
use sha2::Sha256;
use sqlx::AssertSqlSafe;
use std::path::Path;
use std::path::PathBuf;
use tokio::io::AsyncReadExt;

#[path = "migration/destination.rs"]
pub(crate) mod destination_validation;
#[path = "migration/finalize.rs"]
mod finalize;
#[path = "migration/import_memory.rs"]
mod import_memory;
#[path = "migration/import_operational.rs"]
mod import_operational;
#[path = "migration/import_threads.rs"]
mod import_threads;
#[path = "migration/orchestrate.rs"]
mod orchestrate;
#[path = "migration/preflight.rs"]
mod preflight;
#[path = "migration/progress.rs"]
mod progress;
#[path = "migration/snapshot_memory.rs"]
mod snapshot_memory;
#[path = "migration/snapshot_operational.rs"]
mod snapshot_operational;
#[path = "migration/source_lock.rs"]
mod source_lock;
#[path = "migration/source_validation.rs"]
mod source_validation;
#[path = "migration/thread_evidence.rs"]
mod thread_evidence;
#[path = "migration/thread_snapshot.rs"]
mod thread_snapshot;

pub use finalize::RuntimeStateMigrationReport;
pub(crate) use finalize::validate_global_integrity;
pub use import_memory::import_runtime_state_memory;
pub use import_operational::import_runtime_state_operational;
pub use import_threads::import_runtime_state_threads;
pub use orchestrate::migrate_runtime_state;
pub(crate) use progress::RuntimeStateMigrationEvidence;
pub use progress::RuntimeStateMigrationPhase;
pub use progress::RuntimeStateMigrationProgress;
pub(crate) use progress::namespace_digest;
pub(crate) use progress::phase_evidence;
pub use thread_snapshot::BackfillCoordinationSnapshot;
pub use thread_snapshot::CanonicalThreadHistoryReader;
pub use thread_snapshot::CanonicalThreadHistorySnapshot;
pub use thread_snapshot::RuntimeStateThreadSnapshot;
pub use thread_snapshot::ThreadHistoryProjectionStateSnapshot;
pub use thread_snapshot::ThreadItemProjectionSnapshot;
pub use thread_snapshot::ThreadMigrationSnapshot;
pub use thread_snapshot::ThreadSpawnEdgeSnapshot;
pub use thread_snapshot::ThreadTurnProjectionSnapshot;
pub use thread_snapshot::snapshot_runtime_state_migration_threads;

/// Materializes rebuildable PostgreSQL thread projections during migration.
///
/// Implementations must derive every projection from the Canonical Thread History in `snapshot`
/// and use `connection` for every write so projection materialization remains part of the import
/// transaction. This keeps the migration crate independent of a concrete thread-store backend
/// while allowing that backend to reuse its normal projection logic.
pub trait RuntimeStateThreadProjectionMaterializer {
    type Error: std::error::Error + Send + Sync + 'static;

    /// PostgreSQL schema that receives the materialized projections.
    fn destination_schema(&self) -> &str;

    fn materialize<'a>(
        &'a self,
        connection: &'a mut sqlx::PgConnection,
        snapshot: &'a RuntimeStateThreadSnapshot,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'a;
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceInventory {
    databases: Vec<SqliteDatabaseInventory>,
    rollout_files: Vec<SourceFileInventory>,
    memory_files: Vec<SourceFileInventory>,
    imported_resources: Vec<SourceFileInventory>,
    configuration: Option<SourceFileInventory>,
    session_index: Option<SourceFileInventory>,
}

/// Complete read-only inventory produced before an offline Runtime State Migration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeStateMigrationInventory {
    databases: Vec<SqliteDatabaseInventory>,
    rollout_files: Vec<SourceFileInventory>,
    memory_files: Vec<SourceFileInventory>,
    imported_resources: Vec<SourceFileInventory>,
    configuration: Option<SourceFileInventory>,
    session_index: Option<SourceFileInventory>,
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

    pub fn configuration(&self) -> Option<&SourceFileInventory> {
        self.configuration.as_ref()
    }

    pub fn session_index(&self) -> Option<&SourceFileInventory> {
        self.session_index.as_ref()
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
/// Source authorities are inventoried twice and the destination is revalidated after that work.
/// Because no cross-database lock can make these checks atomic, a write beginning after the final
/// validation can still escape detection. Keep the source offline and the destination isolated
/// from before preflight starts until the later migration operation completes.
pub async fn preflight_runtime_state_migration(
    source: SqliteConfig,
    destination: PostgresNamespaceConfig,
) -> anyhow::Result<RuntimeStateMigrationInventory> {
    let destination_state = destination_validation::inspect(&destination).await?;
    let source_inventory = inspect_source(&source).await?;
    let verification_inventory = inspect_source(&source).await?;
    anyhow::ensure!(
        source_inventory == verification_inventory,
        "Runtime State Migration source changed during preflight; stop every process using this SQLite home and retry"
    );
    let verified_destination_state = destination_validation::inspect(&destination).await?;
    anyhow::ensure!(
        destination_state == verified_destination_state,
        "PostgreSQL Runtime State Namespace changed during preflight; isolate the empty destination and retry"
    );
    let SourceInventory {
        databases,
        rollout_files,
        memory_files,
        imported_resources,
        configuration,
        session_index,
    } = source_inventory;

    Ok(RuntimeStateMigrationInventory {
        databases,
        rollout_files,
        memory_files,
        imported_resources,
        configuration,
        session_index,
        destination_schema: destination_state.schema,
        destination_schema_version: destination_state.version,
    })
}

async fn inspect_source(source: &SqliteConfig) -> anyhow::Result<SourceInventory> {
    let databases = inspect_sqlite_databases(source).await?;
    let rollout_files = collect_files(source.home(), &["sessions", "archived_sessions"], |path| {
        source_validation::logical_rollout_path(path).is_some()
    })
    .await?;
    let mut physical_by_logical_path = std::collections::HashMap::new();
    for file in &rollout_files {
        let Some(logical_path) = source_validation::logical_rollout_path(&file.relative_path)
        else {
            continue;
        };
        if let Some(existing) = physical_by_logical_path.insert(logical_path, &file.relative_path) {
            anyhow::bail!(
                "Runtime State Migration source has ambiguous physical rollout files {} and {}; remove or reconcile one copy and retry",
                existing.display(),
                file.relative_path.display()
            );
        }
    }
    let memory_artifacts = collect_files(source.home(), &["memories"], |_| true).await?;
    let imported_resource_prefix = Path::new("memories")
        .join("extensions")
        .join("external_agent_import");
    let (imported_resources, memory_files) = memory_artifacts
        .into_iter()
        .partition(|file| file.relative_path.starts_with(&imported_resource_prefix));
    source_validation::validate_rollout_files(source, &rollout_files).await?;
    let configuration = if tokio::fs::try_exists(source.home().join("config.toml")).await? {
        Some(inventory_file(source.home(), Path::new("config.toml")).await?)
    } else {
        None
    };
    let session_index = if tokio::fs::try_exists(source.home().join("session_index.jsonl")).await? {
        Some(inventory_file(source.home(), Path::new("session_index.jsonl")).await?)
    } else {
        None
    };
    Ok(SourceInventory {
        databases,
        rollout_files,
        memory_files,
        imported_resources,
        configuration,
        session_index,
    })
}

async fn inspect_sqlite_databases(
    source: &SqliteConfig,
) -> anyhow::Result<Vec<SqliteDatabaseInventory>> {
    let mut databases = Vec::new();
    for database in source.runtime_db_paths() {
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
            source_validation::validate_database_schema(database.label, &pool, &table_names)
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
                pending.push(path);
            } else if metadata.is_file() && include(&path) {
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
#[path = "migration_destination_tests.rs"]
mod destination_tests;
#[cfg(test)]
#[path = "migration_memory_contract_tests.rs"]
mod memory_contract_tests;
#[cfg(test)]
#[path = "migration_operational_contract_tests.rs"]
mod operational_contract_tests;
#[cfg(test)]
#[path = "migration_redaction_tests.rs"]
mod redaction_tests;
#[cfg(test)]
#[path = "migration_source_tests.rs"]
mod source_tests;
#[cfg(test)]
#[path = "migration_test_support.rs"]
mod test_support;
#[cfg(test)]
#[path = "migration_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "migration_thread_snapshot_tests.rs"]
mod thread_snapshot_tests;
