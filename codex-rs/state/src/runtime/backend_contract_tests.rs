use super::RemoteControlEnrollmentRecord;
use super::RuntimeStateBackendConfig;
use super::StateRuntime;
use super::test_support::unique_temp_dir;
use crate::PostgresNamespaceAction;
use crate::PostgresNamespaceConfig;
use crate::SqliteConfig;
use crate::postgres::test_support::PostgresContractFixture;
use crate::postgres::test_support::test_database_url;
use anyhow::Result;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::sync::Arc;

enum RuntimeStateBackendFixture {
    Postgresql {
        codex_home: AbsolutePathBuf,
        namespace: PostgresNamespaceConfig,
    },
    Sqlite(SqliteConfig),
}

impl RuntimeStateBackendFixture {
    async fn construct(self) -> Result<Arc<StateRuntime>> {
        match self {
            Self::Postgresql {
                codex_home,
                namespace,
            } => {
                StateRuntime::init_with_backend(
                    RuntimeStateBackendConfig::Postgresql {
                        codex_home,
                        namespace,
                    },
                    "test-provider".to_string(),
                )
                .await
            }
            Self::Sqlite(sqlite) => {
                StateRuntime::init_with_backend(
                    RuntimeStateBackendConfig::Sqlite(sqlite),
                    "test-provider".to_string(),
                )
                .await
            }
        }
    }
}

async fn run_runtime_state_smoke_contract(fixture: RuntimeStateBackendFixture) -> Result<()> {
    let expected_local_rollout_history = matches!(&fixture, RuntimeStateBackendFixture::Sqlite(_));
    let runtime = fixture.construct().await?;
    assert_eq!(
        runtime.uses_local_rollout_history(),
        expected_local_rollout_history
    );
    runtime.backfill_coordinator().state().await?;
    assert_eq!(
        runtime
            .find_rollout_path_by_id(ThreadId::new(), /*archived_only*/ None)
            .await?,
        None
    );

    let enrollment = RemoteControlEnrollmentRecord {
        websocket_url: "wss://example.com/runtime-state-contract".to_string(),
        account_id: "contract-account".to_string(),
        app_server_client_name: Some("contract-client".to_string()),
        server_id: "contract-server".to_string(),
        environment_id: "contract-environment".to_string(),
        server_name: "Runtime State Contract".to_string(),
        remote_control_enabled: Some(true),
    };
    runtime
        .upsert_remote_control_enrollment(&enrollment)
        .await?;
    let persisted_enrollment = runtime
        .get_remote_control_enrollment(
            &enrollment.websocket_url,
            &enrollment.account_id,
            enrollment.app_server_client_name.as_deref(),
        )
        .await?;

    runtime.close().await;
    assert_eq!(persisted_enrollment, Some(enrollment));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_satisfies_runtime_state_smoke_without_local_filesystem_access()
-> Result<()> {
    let database_url = test_database_url()?;
    let mut postgres = PostgresContractFixture::new(database_url, "runtime_backend")?;
    postgres.manage(PostgresNamespaceAction::Migrate).await?;
    postgres.mark_runtime_ready_for_tests().await?;
    let codex_home = unique_temp_dir();
    assert!(!codex_home.exists());

    run_runtime_state_smoke_contract(RuntimeStateBackendFixture::Postgresql {
        codex_home: AbsolutePathBuf::try_from(codex_home.as_path())?,
        namespace: postgres.config_for_tests(),
    })
    .await?;

    assert!(!codex_home.exists());
    postgres.cleanup().await
}

#[tokio::test]
async fn sqlite_satisfies_runtime_state_smoke_contract() -> Result<()> {
    let sqlite_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(sqlite_home)?);

    run_runtime_state_smoke_contract(RuntimeStateBackendFixture::Sqlite(sqlite)).await
}
