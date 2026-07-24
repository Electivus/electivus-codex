use super::StateRuntime;
use crate::LogEntry;
use crate::LogQuery;
use crate::LogRow;
use sqlx::PgPool;
use sqlx::SqlitePool;
use std::sync::Arc;

#[path = "log_store/common.rs"]
mod common;
#[path = "logs/postgres.rs"]
mod postgres;
#[path = "logs/sqlite.rs"]
mod sqlite;

use common::LOG_RETENTION_DAYS;
use common::estimated_log_bytes;
use common::format_feedback_log_line;
use postgres::PostgresLogStore;
use sqlite::SqliteLogStore;

/// Storage-neutral facade for runtime log operations.
///
/// Backends own their SQL and row decoding.
#[derive(Clone)]
pub(crate) struct LogStore {
    backend: LogStoreBackend,
}

#[derive(Clone)]
enum LogStoreBackend {
    Postgres(PostgresLogStore),
    Sqlite(SqliteLogStore),
}

impl LogStore {
    pub(crate) fn from_sqlite(pool: Arc<SqlitePool>) -> Self {
        Self {
            backend: LogStoreBackend::Sqlite(SqliteLogStore::new(pool)),
        }
    }

    pub(crate) fn from_postgres(pool: PgPool, schema: String) -> Self {
        Self {
            backend: LogStoreBackend::Postgres(PostgresLogStore::new(pool, schema)),
        }
    }

    pub(crate) async fn insert_log(&self, entry: &LogEntry) -> anyhow::Result<()> {
        match &self.backend {
            LogStoreBackend::Postgres(store) => store.insert_log(entry).await,
            LogStoreBackend::Sqlite(store) => store.insert_log(entry).await,
        }
    }

    pub(crate) async fn insert_logs(&self, entries: &[LogEntry]) -> anyhow::Result<()> {
        match &self.backend {
            LogStoreBackend::Postgres(store) => store.insert_logs(entries).await,
            LogStoreBackend::Sqlite(store) => store.insert_logs(entries).await,
        }
    }

    pub(crate) async fn run_startup_maintenance(&self) -> anyhow::Result<()> {
        match &self.backend {
            LogStoreBackend::Postgres(store) => store.run_startup_maintenance().await,
            LogStoreBackend::Sqlite(store) => store.run_logs_startup_maintenance().await,
        }
    }

    pub(crate) async fn query_logs(&self, query: &LogQuery) -> anyhow::Result<Vec<LogRow>> {
        match &self.backend {
            LogStoreBackend::Postgres(store) => store.query_logs(query).await,
            LogStoreBackend::Sqlite(store) => store.query_logs(query).await,
        }
    }

    pub(crate) async fn query_feedback_logs_for_threads(
        &self,
        thread_ids: &[&str],
    ) -> anyhow::Result<Vec<u8>> {
        match &self.backend {
            LogStoreBackend::Postgres(store) => {
                store.query_feedback_logs_for_threads(thread_ids).await
            }
            LogStoreBackend::Sqlite(store) => {
                store.query_feedback_logs_for_threads(thread_ids).await
            }
        }
    }

    pub(crate) async fn query_feedback_logs(&self, thread_id: &str) -> anyhow::Result<Vec<u8>> {
        self.query_feedback_logs_for_threads(&[thread_id]).await
    }

    pub(crate) async fn max_log_id(&self, query: &LogQuery) -> anyhow::Result<i64> {
        match &self.backend {
            LogStoreBackend::Postgres(store) => store.max_log_id(query).await,
            LogStoreBackend::Sqlite(store) => store.max_log_id(query).await,
        }
    }

    pub(crate) async fn delete_logs_for_thread(&self, thread_id: &str) -> anyhow::Result<()> {
        match &self.backend {
            LogStoreBackend::Postgres(store) => store.delete_logs_for_thread(thread_id).await,
            LogStoreBackend::Sqlite(store) => store.delete_logs_for_thread(thread_id).await,
        }
    }

    pub(crate) async fn close(&self) {
        match &self.backend {
            LogStoreBackend::Postgres(store) => store.close().await,
            LogStoreBackend::Sqlite(store) => store.close().await,
        }
    }
}

impl StateRuntime {
    pub async fn insert_log(&self, entry: &LogEntry) -> anyhow::Result<()> {
        self.logs.insert_log(entry).await
    }

    pub async fn insert_logs(&self, entries: &[LogEntry]) -> anyhow::Result<()> {
        self.logs.insert_logs(entries).await
    }

    pub(crate) async fn run_logs_startup_maintenance(&self) -> anyhow::Result<()> {
        self.logs.run_startup_maintenance().await
    }

    pub async fn query_logs(&self, query: &LogQuery) -> anyhow::Result<Vec<LogRow>> {
        self.logs.query_logs(query).await
    }

    pub async fn query_feedback_logs_for_threads(
        &self,
        thread_ids: &[&str],
    ) -> anyhow::Result<Vec<u8>> {
        self.logs.query_feedback_logs_for_threads(thread_ids).await
    }

    pub async fn query_feedback_logs(&self, thread_id: &str) -> anyhow::Result<Vec<u8>> {
        self.logs.query_feedback_logs(thread_id).await
    }

    pub async fn max_log_id(&self, query: &LogQuery) -> anyhow::Result<i64> {
        self.logs.max_log_id(query).await
    }
}
