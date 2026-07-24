use super::CanonicalThreadHistoryReader;
use super::RuntimeStateMigrationReport;
use super::RuntimeStateThreadProjectionMaterializer;
use super::finalize::finalize_runtime_state_migration;
use super::import_runtime_state_memory;
use super::import_runtime_state_operational;
use super::import_runtime_state_threads;
use super::preflight::preflight_or_resume_runtime_state_migration;
use crate::PostgresNamespaceConfig;
use crate::SqliteConfig;

/// Run the complete explicit, offline Runtime State Migration phase chain.
pub async fn migrate_runtime_state(
    source: SqliteConfig,
    destination: PostgresNamespaceConfig,
    history_reader: &impl CanonicalThreadHistoryReader,
    projection_materializer: &impl RuntimeStateThreadProjectionMaterializer,
) -> anyhow::Result<RuntimeStateMigrationReport> {
    let inventory =
        preflight_or_resume_runtime_state_migration(source.clone(), destination.clone()).await?;
    import_runtime_state_threads(
        &source,
        &destination,
        &inventory,
        history_reader,
        projection_materializer,
    )
    .await?;
    import_runtime_state_operational(&source, &destination, &inventory).await?;
    import_runtime_state_memory(&source, &destination, &inventory).await?;
    finalize_runtime_state_migration(&source, &destination, &inventory).await
}
