use super::StateRuntime;
use sqlx::PgPool;
use sqlx::SqlitePool;
use std::sync::Arc;

mod postgres;
mod sqlite;

use postgres::PostgresRemoteControlEnrollmentStore;
use sqlite::SqliteRemoteControlEnrollmentStore;

const REMOTE_CONTROL_APP_SERVER_CLIENT_NAME_NONE: &str = "";

/// Persisted remote-control server enrollment, including the lookup key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteControlEnrollmentRecord {
    pub websocket_url: String,
    pub account_id: String,
    pub app_server_client_name: Option<String>,
    pub server_id: String,
    pub environment_id: String,
    pub server_name: String,
    pub remote_control_enabled: Option<bool>,
}

fn app_server_client_name_key(app_server_client_name: Option<&str>) -> &str {
    app_server_client_name.unwrap_or(REMOTE_CONTROL_APP_SERVER_CLIENT_NAME_NONE)
}

fn app_server_client_name_from_key(app_server_client_name: String) -> Option<String> {
    if app_server_client_name.is_empty() {
        None
    } else {
        Some(app_server_client_name)
    }
}

/// Storage-neutral facade for persisted remote-control enrollment and enabled state.
///
/// Backends keep SQL and row decoding private while callers observe one runtime-state contract.
#[derive(Clone)]
pub struct RemoteControlEnrollmentStore {
    backend: RemoteControlEnrollmentStoreBackend,
}

#[derive(Clone)]
enum RemoteControlEnrollmentStoreBackend {
    Postgres(PostgresRemoteControlEnrollmentStore),
    Sqlite(SqliteRemoteControlEnrollmentStore),
}

impl RemoteControlEnrollmentStore {
    pub(crate) fn from_sqlite(pool: Arc<SqlitePool>) -> Self {
        Self {
            backend: RemoteControlEnrollmentStoreBackend::Sqlite(
                SqliteRemoteControlEnrollmentStore::new(pool),
            ),
        }
    }

    pub(crate) fn from_postgres(pool: PgPool, schema: String) -> Self {
        Self {
            backend: RemoteControlEnrollmentStoreBackend::Postgres(
                PostgresRemoteControlEnrollmentStore::new(pool, schema),
            ),
        }
    }

    pub async fn get(
        &self,
        websocket_url: &str,
        account_id: &str,
        app_server_client_name: Option<&str>,
    ) -> anyhow::Result<Option<RemoteControlEnrollmentRecord>> {
        let result = match &self.backend {
            RemoteControlEnrollmentStoreBackend::Postgres(store) => {
                store
                    .get(websocket_url, account_id, app_server_client_name)
                    .await
            }
            RemoteControlEnrollmentStoreBackend::Sqlite(store) => {
                store
                    .get(websocket_url, account_id, app_server_client_name)
                    .await
            }
        };
        result.map_err(|_| enrollment_persistence_error("get remote control enrollment"))
    }

    pub async fn upsert(&self, enrollment: &RemoteControlEnrollmentRecord) -> anyhow::Result<()> {
        let result = match &self.backend {
            RemoteControlEnrollmentStoreBackend::Postgres(store) => store.upsert(enrollment).await,
            RemoteControlEnrollmentStoreBackend::Sqlite(store) => store.upsert(enrollment).await,
        };
        result.map_err(|_| enrollment_persistence_error("upsert remote control enrollment"))
    }

    pub async fn set_enabled(
        &self,
        websocket_url: &str,
        account_id: &str,
        app_server_client_name: Option<&str>,
        remote_control_enabled: bool,
    ) -> anyhow::Result<u64> {
        let result = match &self.backend {
            RemoteControlEnrollmentStoreBackend::Postgres(store) => {
                store
                    .set_enabled(
                        websocket_url,
                        account_id,
                        app_server_client_name,
                        remote_control_enabled,
                    )
                    .await
            }
            RemoteControlEnrollmentStoreBackend::Sqlite(store) => {
                store
                    .set_enabled(
                        websocket_url,
                        account_id,
                        app_server_client_name,
                        remote_control_enabled,
                    )
                    .await
            }
        };
        result.map_err(|_| enrollment_persistence_error("set remote control enabled"))
    }

    pub async fn delete(
        &self,
        websocket_url: &str,
        account_id: &str,
        app_server_client_name: Option<&str>,
    ) -> anyhow::Result<u64> {
        let result = match &self.backend {
            RemoteControlEnrollmentStoreBackend::Postgres(store) => {
                store
                    .delete(websocket_url, account_id, app_server_client_name)
                    .await
            }
            RemoteControlEnrollmentStoreBackend::Sqlite(store) => {
                store
                    .delete(websocket_url, account_id, app_server_client_name)
                    .await
            }
        };
        result.map_err(|_| enrollment_persistence_error("delete remote control enrollment"))
    }

    #[cfg(test)]
    pub(crate) async fn close(&self) {
        match &self.backend {
            RemoteControlEnrollmentStoreBackend::Postgres(store) => store.close().await,
            RemoteControlEnrollmentStoreBackend::Sqlite(store) => store.close().await,
        }
    }
}

fn enrollment_persistence_error(operation: &'static str) -> anyhow::Error {
    anyhow::anyhow!(
        "Runtime State could not complete the `{operation}` operation; verify enrollment persistence health, then retry"
    )
}

impl StateRuntime {
    pub fn remote_control_enrollment_store(&self) -> RemoteControlEnrollmentStore {
        self.remote_control_enrollments.clone()
    }

    pub async fn get_remote_control_enrollment(
        &self,
        websocket_url: &str,
        account_id: &str,
        app_server_client_name: Option<&str>,
    ) -> anyhow::Result<Option<RemoteControlEnrollmentRecord>> {
        self.remote_control_enrollments
            .get(websocket_url, account_id, app_server_client_name)
            .await
    }

    pub async fn upsert_remote_control_enrollment(
        &self,
        enrollment: &RemoteControlEnrollmentRecord,
    ) -> anyhow::Result<()> {
        self.remote_control_enrollments.upsert(enrollment).await
    }

    pub async fn set_remote_control_enabled(
        &self,
        websocket_url: &str,
        account_id: &str,
        app_server_client_name: Option<&str>,
        remote_control_enabled: bool,
    ) -> anyhow::Result<u64> {
        self.remote_control_enrollments
            .set_enabled(
                websocket_url,
                account_id,
                app_server_client_name,
                remote_control_enabled,
            )
            .await
    }

    pub async fn delete_remote_control_enrollment(
        &self,
        websocket_url: &str,
        account_id: &str,
        app_server_client_name: Option<&str>,
    ) -> anyhow::Result<u64> {
        self.remote_control_enrollments
            .delete(websocket_url, account_id, app_server_client_name)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::unique_temp_dir;
    use super::RemoteControlEnrollmentRecord;
    use super::StateRuntime;
    use crate::migrations::STATE_MIGRATOR;
    use codex_utils_absolute_path::test_support::PathExt;
    use pretty_assertions::assert_eq;
    use sqlx::migrate::Migrator;
    use std::borrow::Cow;

    #[tokio::test]
    async fn remote_control_enrollment_round_trips_by_target_and_account() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(
            crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
            "test-provider".to_string(),
        )
        .await
        .expect("initialize runtime");

        runtime
            .upsert_remote_control_enrollment(&RemoteControlEnrollmentRecord {
                websocket_url: "wss://example.com/backend-api/wham/remote/control/server"
                    .to_string(),
                account_id: "account-a".to_string(),
                app_server_client_name: Some("desktop-client".to_string()),
                server_id: "srv_e_first".to_string(),
                environment_id: "env_first".to_string(),
                server_name: "first-server".to_string(),
                remote_control_enabled: Some(false),
            })
            .await
            .expect("insert first enrollment");
        runtime
            .upsert_remote_control_enrollment(&RemoteControlEnrollmentRecord {
                websocket_url: "wss://example.com/backend-api/wham/remote/control/server"
                    .to_string(),
                account_id: "account-b".to_string(),
                app_server_client_name: Some("desktop-client".to_string()),
                server_id: "srv_e_second".to_string(),
                environment_id: "env_second".to_string(),
                server_name: "second-server".to_string(),
                remote_control_enabled: Some(true),
            })
            .await
            .expect("insert second enrollment");

        assert_eq!(
            runtime
                .get_remote_control_enrollment(
                    "wss://example.com/backend-api/wham/remote/control/server",
                    "account-a",
                    Some("desktop-client"),
                )
                .await
                .expect("load first enrollment"),
            Some(RemoteControlEnrollmentRecord {
                websocket_url: "wss://example.com/backend-api/wham/remote/control/server"
                    .to_string(),
                account_id: "account-a".to_string(),
                app_server_client_name: Some("desktop-client".to_string()),
                server_id: "srv_e_first".to_string(),
                environment_id: "env_first".to_string(),
                server_name: "first-server".to_string(),
                remote_control_enabled: Some(false),
            })
        );
        assert_eq!(
            runtime
                .get_remote_control_enrollment(
                    "wss://example.com/backend-api/wham/remote/control/server",
                    "account-missing",
                    Some("desktop-client"),
                )
                .await
                .expect("load missing enrollment"),
            None
        );
        assert_eq!(
            runtime
                .get_remote_control_enrollment(
                    "wss://example.com/backend-api/wham/remote/control/server",
                    "account-a",
                    Some("other-client"),
                )
                .await
                .expect("load wrong client enrollment"),
            None
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn delete_remote_control_enrollment_removes_only_matching_entry() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(
            crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
            "test-provider".to_string(),
        )
        .await
        .expect("initialize runtime");

        runtime
            .upsert_remote_control_enrollment(&RemoteControlEnrollmentRecord {
                websocket_url: "wss://example.com/backend-api/wham/remote/control/server"
                    .to_string(),
                account_id: "account-a".to_string(),
                app_server_client_name: None,
                server_id: "srv_e_first".to_string(),
                environment_id: "env_first".to_string(),
                server_name: "first-server".to_string(),
                remote_control_enabled: Some(false),
            })
            .await
            .expect("insert first enrollment");
        runtime
            .upsert_remote_control_enrollment(&RemoteControlEnrollmentRecord {
                websocket_url: "wss://example.com/backend-api/wham/remote/control/server"
                    .to_string(),
                account_id: "account-b".to_string(),
                app_server_client_name: None,
                server_id: "srv_e_second".to_string(),
                environment_id: "env_second".to_string(),
                server_name: "second-server".to_string(),
                remote_control_enabled: Some(true),
            })
            .await
            .expect("insert second enrollment");

        assert_eq!(
            runtime
                .delete_remote_control_enrollment(
                    "wss://example.com/backend-api/wham/remote/control/server",
                    "account-a",
                    /*app_server_client_name*/ None,
                )
                .await
                .expect("delete first enrollment"),
            1
        );
        assert_eq!(
            runtime
                .get_remote_control_enrollment(
                    "wss://example.com/backend-api/wham/remote/control/server",
                    "account-a",
                    /*app_server_client_name*/ None,
                )
                .await
                .expect("load deleted enrollment"),
            None
        );
        assert_eq!(
            runtime
                .get_remote_control_enrollment(
                    "wss://example.com/backend-api/wham/remote/control/server",
                    "account-b",
                    /*app_server_client_name*/ None,
                )
                .await
                .expect("load retained enrollment"),
            Some(RemoteControlEnrollmentRecord {
                websocket_url: "wss://example.com/backend-api/wham/remote/control/server"
                    .to_string(),
                account_id: "account-b".to_string(),
                app_server_client_name: None,
                server_id: "srv_e_second".to_string(),
                environment_id: "env_second".to_string(),
                server_name: "second-server".to_string(),
                remote_control_enabled: Some(true),
            })
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn remote_control_facade_errors_are_backend_independent_and_sanitized() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init_sqlite(codex_home, "test-provider".to_string())
            .await
            .expect("initialize runtime");
        let store = runtime.remote_control_enrollment_store();
        store.close().await;

        let error = store
            .get(
                "wss://example.com/backend-api/wham/remote/control/server",
                "account-a",
                /*app_server_client_name*/ None,
            )
            .await
            .expect_err("closed enrollment persistence should fail without backend details");
        let message = error.to_string();
        assert_eq!(
            message,
            "Runtime State could not complete the `get remote control enrollment` operation; verify enrollment persistence health, then retry"
        );
        for backend_term in ["postgres", "sqlite", "sql"] {
            assert!(!message.to_ascii_lowercase().contains(backend_term));
        }
    }

    #[tokio::test]
    async fn migration_preserves_legacy_remote_control_preference_as_null() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex home");
        let old_state_migrator = Migrator {
            migrations: Cow::Owned(
                STATE_MIGRATOR
                    .migrations
                    .iter()
                    .filter(|migration| migration.version <= 36)
                    .cloned()
                    .collect(),
            ),
            ignore_missing: false,
            locking: true,
            no_tx: false,
            table_name: STATE_MIGRATOR.table_name.clone(),
            create_schemas: STATE_MIGRATOR.create_schemas.clone(),
        };
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let pool = sqlite
            .open_read_write_pool(&sqlite.state_db_path())
            .await
            .expect("open old state db");
        old_state_migrator
            .run(&pool)
            .await
            .expect("apply old state schema");
        sqlx::query("INSERT INTO remote_control_enrollments (websocket_url, account_id, app_server_client_name, server_id, environment_id, server_name, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind("wss://example.com/backend-api/wham/remote/control/server")
        .bind("account-a")
        .bind("desktop-client")
        .bind("srv_e_first")
        .bind("env_first")
        .bind("first-server")
        .bind(1_i64)
        .execute(&pool)
        .await
        .expect("insert legacy enrollment");
        pool.close().await;

        let runtime = StateRuntime::init(
            crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
            "test-provider".to_string(),
        )
        .await
        .expect("initialize runtime");
        let actual = runtime
            .get_remote_control_enrollment(
                "wss://example.com/backend-api/wham/remote/control/server",
                "account-a",
                Some("desktop-client"),
            )
            .await
            .expect("load migrated enrollment")
            .expect("legacy enrollment should remain");
        assert_eq!(actual.remote_control_enabled, None);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }
}
