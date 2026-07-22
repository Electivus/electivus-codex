use super::StateRuntime;
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
    Postgres(PostgresExternalAgentConfigImportStore),
    Sqlite(SqliteExternalAgentConfigImportStore),
}

impl ExternalAgentConfigImportStore {
    pub(crate) fn from_sqlite(pool: Arc<SqlitePool>) -> Self {
        Self {
            backend: ExternalAgentConfigImportStoreBackend::Sqlite(
                SqliteExternalAgentConfigImportStore::new(pool),
            ),
        }
    }

    pub(crate) fn from_postgres(pool: PgPool, schema: String) -> Self {
        Self {
            backend: ExternalAgentConfigImportStoreBackend::Postgres(
                PostgresExternalAgentConfigImportStore::new(pool, schema),
            ),
        }
    }

    pub async fn record_completed(
        &self,
        import_id: &str,
        successes: &[ExternalAgentConfigImportSuccessRecord],
        failures: &[ExternalAgentConfigImportFailureRecord],
    ) -> anyhow::Result<()> {
        match &self.backend {
            ExternalAgentConfigImportStoreBackend::Postgres(store) => {
                store.record_completed(import_id, successes, failures).await
            }
            ExternalAgentConfigImportStoreBackend::Sqlite(store) => {
                store.record_completed(import_id, successes, failures).await
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
            ExternalAgentConfigImportStoreBackend::Sqlite(store) => store.details(import_id).await,
        }
    }

    pub async fn history(&self) -> anyhow::Result<Vec<ExternalAgentConfigImportHistoryRecord>> {
        match &self.backend {
            ExternalAgentConfigImportStoreBackend::Postgres(store) => store.history().await,
            ExternalAgentConfigImportStoreBackend::Sqlite(store) => store.history().await,
        }
    }
}

impl StateRuntime {
    pub fn external_agent_config_import_store(&self) -> &ExternalAgentConfigImportStore {
        &self.external_agent_config_imports
    }

    pub async fn record_external_agent_config_import_completed(
        &self,
        import_id: &str,
        successes: &[ExternalAgentConfigImportSuccessRecord],
        failures: &[ExternalAgentConfigImportFailureRecord],
    ) -> anyhow::Result<()> {
        self.external_agent_config_imports
            .record_completed(import_id, successes, failures)
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
