use super::StateRuntime;
use super::memory_store::MemoryArtifact;
use super::memory_store::MemoryArtifactSet;
use super::memory_store::MemoryStore;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sqlx::PgPool;
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;

#[path = "external_agent_config_imports/postgres.rs"]
mod postgres;
#[path = "external_agent_config_imports/sqlite.rs"]
mod sqlite;

use postgres::PostgresExternalAgentConfigImportStore;
use sqlite::SqliteExternalAgentConfigImportStore;

pub(crate) const IMPORTED_MEMORY_EXTENSION_PREFIX: &str = "extensions/external_agent_import/";
pub(crate) const IMPORTED_MEMORY_INSTRUCTIONS_PATH: &str =
    "extensions/external_agent_import/instructions.md";
pub(crate) const IMPORTED_MEMORY_RESOURCES_PREFIX: &str =
    "extensions/external_agent_import/resources/";

/// A validated replacement of selected external-agent project resources.
///
/// Project keys and artifact paths are portable persisted identifiers, not host filesystem paths.
/// A selected project without artifacts represents removal of its previously imported resources.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalAgentMemoryImport {
    project_keys: Vec<String>,
    artifacts: MemoryArtifactSet,
}

impl ExternalAgentMemoryImport {
    pub fn new(
        mut project_keys: Vec<String>,
        artifacts: MemoryArtifactSet,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !project_keys.is_empty(),
            "external-agent Memory Artifact publication requires a selected project"
        );
        project_keys.sort();
        project_keys.dedup();
        anyhow::ensure!(
            project_keys
                .iter()
                .all(|project_key| !project_key.contains('/')),
            "external-agent memory project keys must be one portable path component"
        );
        MemoryArtifactSet::new(
            project_keys
                .iter()
                .map(|project_key| {
                    MemoryArtifact::new(
                        format!("{IMPORTED_MEMORY_RESOURCES_PREFIX}{project_key}/scope.json"),
                        Vec::new(),
                    )
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
        )?;
        for artifact in artifacts.artifacts() {
            anyhow::ensure!(
                artifact.path() == IMPORTED_MEMORY_INSTRUCTIONS_PATH
                    || project_keys.iter().any(|project_key| {
                        artifact.path().starts_with(&format!(
                            "{IMPORTED_MEMORY_RESOURCES_PREFIX}{project_key}/"
                        ))
                    }),
                "external-agent Memory Artifact is outside the selected project replacements"
            );
        }
        Ok(Self {
            project_keys,
            artifacts,
        })
    }

    pub fn project_keys(&self) -> &[String] {
        &self.project_keys
    }

    pub fn artifacts(&self) -> &MemoryArtifactSet {
        &self.artifacts
    }

    /// Overlays a later replacement while retaining unrelated project resources.
    pub fn overlay(self, newer: Self) -> anyhow::Result<Self> {
        let Self {
            mut project_keys,
            artifacts,
        } = self;
        let Self {
            project_keys: newer_project_keys,
            artifacts: newer_artifacts,
        } = newer;
        let mut merged_artifacts = artifacts
            .artifacts()
            .iter()
            .filter(|artifact| {
                !newer_project_keys.iter().any(|project_key| {
                    artifact
                        .path()
                        .starts_with(&format!("{IMPORTED_MEMORY_RESOURCES_PREFIX}{project_key}/"))
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        for artifact in newer_artifacts.artifacts() {
            merged_artifacts.retain(|existing| existing.path() != artifact.path());
            merged_artifacts.push(artifact.clone());
        }
        project_keys.extend(newer_project_keys);
        Self::new(project_keys, MemoryArtifactSet::new(merged_artifacts)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAgentConfigImportSuccessRecord {
    pub item_type: String,
    pub cwd: Option<PathBuf>,
    pub source: Option<String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAgentConfigImportFailureRecord {
    pub item_type: String,
    pub error_type: Option<String>,
    #[serde(default)]
    pub sub_error_type: Option<String>,
    pub failure_stage: String,
    pub message: String,
    pub cwd: Option<PathBuf>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAgentConfigImportDetailsRecord {
    pub successes: Vec<ExternalAgentConfigImportSuccessRecord>,
    pub failures: Vec<ExternalAgentConfigImportFailureRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAgentConfigImportHistoryRecord {
    pub import_id: String,
    pub provider_id: Option<String>,
    pub completed_at_ms: i64,
    pub successes: Vec<ExternalAgentConfigImportSuccessRecord>,
    pub failures: Vec<ExternalAgentConfigImportFailureRecord>,
}

/// Storage-neutral facade for completed external-agent import outcomes and history.
///
/// Implementations preserve the order of success and failure payloads exactly as supplied. A
/// repeated import identifier replaces the prior completion as one record rather than adding a
/// duplicate history entry.
#[derive(Clone)]
pub struct ExternalAgentConfigImportStore {
    backend: ExternalAgentConfigImportStoreBackend,
}

#[derive(Clone)]
enum ExternalAgentConfigImportStoreBackend {
    Postgres(Box<PostgresExternalAgentConfigImportStore>),
    Sqlite {
        imports: SqliteExternalAgentConfigImportStore,
        memories: MemoryStore,
    },
}

impl ExternalAgentConfigImportStore {
    pub(crate) fn from_sqlite(pool: Arc<SqlitePool>, memories: MemoryStore) -> Self {
        Self {
            backend: ExternalAgentConfigImportStoreBackend::Sqlite {
                imports: SqliteExternalAgentConfigImportStore::new(pool),
                memories,
            },
        }
    }

    pub(crate) fn from_postgres(pool: PgPool, schema: String) -> Self {
        Self {
            backend: ExternalAgentConfigImportStoreBackend::Postgres(Box::new(
                PostgresExternalAgentConfigImportStore::new(pool, schema),
            )),
        }
    }

    pub async fn record_completed(
        &self,
        import_id: &str,
        successes: &[ExternalAgentConfigImportSuccessRecord],
        failures: &[ExternalAgentConfigImportFailureRecord],
    ) -> anyhow::Result<()> {
        self.record_completed_with_provider(
            import_id, /*provider_id*/ None, successes, failures,
        )
        .await
    }

    pub async fn record_completed_with_provider(
        &self,
        import_id: &str,
        provider_id: Option<&str>,
        successes: &[ExternalAgentConfigImportSuccessRecord],
        failures: &[ExternalAgentConfigImportFailureRecord],
    ) -> anyhow::Result<()> {
        match &self.backend {
            ExternalAgentConfigImportStoreBackend::Postgres(store) => {
                store
                    .record_completed(import_id, provider_id, successes, failures)
                    .await
            }
            ExternalAgentConfigImportStoreBackend::Sqlite { imports, .. } => {
                imports
                    .record_completed(import_id, provider_id, successes, failures)
                    .await
            }
        }
    }

    /// Records one completion and schedules or atomically publishes its imported resources,
    /// according to the configured backend's authority model.
    pub async fn record_completed_with_memory_import(
        &self,
        import_id: &str,
        successes: &[ExternalAgentConfigImportSuccessRecord],
        failures: &[ExternalAgentConfigImportFailureRecord],
        memory_import: &ExternalAgentMemoryImport,
    ) -> anyhow::Result<()> {
        self.record_completed_with_memory_import_and_provider(
            import_id,
            /*provider_id*/ None,
            successes,
            failures,
            memory_import,
        )
        .await
    }

    pub async fn record_completed_with_memory_import_and_provider(
        &self,
        import_id: &str,
        provider_id: Option<&str>,
        successes: &[ExternalAgentConfigImportSuccessRecord],
        failures: &[ExternalAgentConfigImportFailureRecord],
        memory_import: &ExternalAgentMemoryImport,
    ) -> anyhow::Result<()> {
        match &self.backend {
            ExternalAgentConfigImportStoreBackend::Postgres(store) => {
                store
                    .record_completed_with_memory_import(
                        import_id,
                        provider_id,
                        successes,
                        failures,
                        memory_import,
                    )
                    .await
            }
            ExternalAgentConfigImportStoreBackend::Sqlite { imports, memories } => {
                imports
                    .record_completed(import_id, provider_id, successes, failures)
                    .await?;
                memories
                    .enqueue_global_consolidation(Utc::now().timestamp())
                    .await
            }
        }
    }

    pub async fn details(
        &self,
        import_id: &str,
    ) -> anyhow::Result<Option<ExternalAgentConfigImportDetailsRecord>> {
        match &self.backend {
            ExternalAgentConfigImportStoreBackend::Postgres(store) => {
                store.details(import_id).await
            }
            ExternalAgentConfigImportStoreBackend::Sqlite { imports, .. } => {
                imports.details(import_id).await
            }
        }
    }

    pub async fn history(&self) -> anyhow::Result<Vec<ExternalAgentConfigImportHistoryRecord>> {
        match &self.backend {
            ExternalAgentConfigImportStoreBackend::Postgres(store) => store.history().await,
            ExternalAgentConfigImportStoreBackend::Sqlite { imports, .. } => {
                imports.history().await
            }
        }
    }
}

impl StateRuntime {
    pub async fn record_external_agent_config_import_completed(
        &self,
        import_id: &str,
        provider_id: Option<&str>,
        successes: &[ExternalAgentConfigImportSuccessRecord],
        failures: &[ExternalAgentConfigImportFailureRecord],
    ) -> anyhow::Result<()> {
        self.external_agent_config_imports
            .record_completed_with_provider(import_id, provider_id, successes, failures)
            .await
    }

    pub async fn record_external_agent_config_import_completed_with_memory_import(
        &self,
        import_id: &str,
        provider_id: Option<&str>,
        successes: &[ExternalAgentConfigImportSuccessRecord],
        failures: &[ExternalAgentConfigImportFailureRecord],
        memory_import: &ExternalAgentMemoryImport,
    ) -> anyhow::Result<()> {
        self.external_agent_config_imports
            .record_completed_with_memory_import_and_provider(
                import_id,
                provider_id,
                successes,
                failures,
                memory_import,
            )
            .await
    }

    pub async fn external_agent_config_import_details_record(
        &self,
        import_id: &str,
    ) -> anyhow::Result<Option<ExternalAgentConfigImportDetailsRecord>> {
        self.external_agent_config_imports.details(import_id).await
    }

    pub async fn external_agent_config_import_history_records(
        &self,
    ) -> anyhow::Result<Vec<ExternalAgentConfigImportHistoryRecord>> {
        self.external_agent_config_imports.history().await
    }
}

#[cfg(test)]
#[path = "external_agent_config_imports_tests.rs"]
mod tests;
