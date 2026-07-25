use super::RuntimeStateMigrationInventory;
use super::SourceInventory;
use super::destination_validation;
use super::import_threads;
use super::inspect_source;
use crate::PostgresNamespaceConfig;
use crate::SqliteConfig;

/// Preflight an empty destination or validate an exact, incomplete migration for resumption.
///
/// Like [`super::preflight_runtime_state_migration`], this is read-only and inventories the
/// quiescent SQLite source twice. A non-empty destination is accepted only when its recorded source
/// identity, source fingerprint, phase evidence, and namespace digest all match the source being
/// resumed.
pub(super) async fn preflight_or_resume_runtime_state_migration(
    source: SqliteConfig,
    destination: PostgresNamespaceConfig,
) -> anyhow::Result<RuntimeStateMigrationInventory> {
    let source_inventory = inspect_source(&source).await?;
    let verification_inventory = inspect_source(&source).await?;
    anyhow::ensure!(
        source_inventory == verification_inventory,
        "Runtime State Migration source changed during preflight; stop every process using this SQLite home and retry"
    );
    let mut inventory = migration_inventory(
        source_inventory,
        destination.schema().to_string(),
        crate::postgres::MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
    );
    let source_identity = import_threads::source_identity(&source);
    let source_fingerprint = import_threads::fingerprint(&inventory);
    let destination_state = destination_validation::inspect_resumable(
        &destination,
        &source_identity,
        &source_fingerprint,
    )
    .await?;
    let verified_destination_state = destination_validation::inspect_resumable(
        &destination,
        &source_identity,
        &source_fingerprint,
    )
    .await?;
    anyhow::ensure!(
        destination_state == verified_destination_state,
        "PostgreSQL Runtime State Namespace changed during preflight; isolate the migration destination and retry"
    );
    inventory.destination_schema = destination_state.schema;
    inventory.destination_schema_version = destination_state.version;
    Ok(inventory)
}

fn migration_inventory(
    source: SourceInventory,
    destination_schema: String,
    destination_schema_version: i64,
) -> RuntimeStateMigrationInventory {
    let SourceInventory {
        databases,
        rollout_files,
        memory_files,
        imported_resources,
        configuration,
        session_index,
    } = source;
    RuntimeStateMigrationInventory {
        databases,
        rollout_files,
        memory_files,
        imported_resources,
        configuration,
        session_index,
        destination_schema,
        destination_schema_version,
    }
}
