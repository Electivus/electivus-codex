use super::RemoteControlEnrollmentRecord;
use super::RuntimeStateBackendConfig;
use super::StateRuntime;
use super::test_support::test_thread_metadata;
use super::test_support::unique_temp_dir;
use crate::SqliteConfig;
use anyhow::Result;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::sync::Arc;

enum RuntimeStateBackendFixture {
    Sqlite(SqliteConfig),
}

impl RuntimeStateBackendFixture {
    async fn construct(self) -> Result<Arc<StateRuntime>> {
        match self {
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
    let runtime = fixture.construct().await?;
    let thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000001201")?;
    let thread = test_thread_metadata(
        runtime.codex_home(),
        thread_id,
        runtime.codex_home().to_path_buf(),
    );
    runtime.upsert_thread(&thread).await?;
    let persisted_thread = runtime.get_thread(thread_id).await?;

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
    assert_eq!(persisted_thread, Some(thread));
    assert_eq!(persisted_enrollment, Some(enrollment));
    Ok(())
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
