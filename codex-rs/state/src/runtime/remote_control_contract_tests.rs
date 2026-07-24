use super::RemoteControlEnrollmentRecord;
use super::RemoteControlEnrollmentStore;
use super::StateRuntime;
use super::test_support::unique_temp_dir;
use anyhow::Result;
use pretty_assertions::assert_eq;

const WEBSOCKET_URL: &str = "wss://example.com/backend-api/wham/remote/control/server";

fn enrollment(
    server_id: &str,
    environment_id: &str,
    server_name: &str,
    remote_control_enabled: Option<bool>,
) -> RemoteControlEnrollmentRecord {
    RemoteControlEnrollmentRecord {
        websocket_url: WEBSOCKET_URL.to_string(),
        account_id: "account-a".to_string(),
        app_server_client_name: None,
        server_id: server_id.to_string(),
        environment_id: environment_id.to_string(),
        server_name: server_name.to_string(),
        remote_control_enabled,
    }
}

async fn load(
    store: &RemoteControlEnrollmentStore,
) -> Result<Option<RemoteControlEnrollmentRecord>> {
    store
        .get(
            WEBSOCKET_URL,
            "account-a",
            /*app_server_client_name*/ None,
        )
        .await
}

pub(crate) async fn run_remote_control_enrollment_contract(
    writer: &RemoteControlEnrollmentStore,
    reader: &RemoteControlEnrollmentStore,
) -> Result<()> {
    assert_eq!(load(reader).await?, None);

    let original = enrollment("server-first", "environment-first", "First", Some(false));
    writer.upsert(&original).await?;
    assert_eq!(load(reader).await?, Some(original.clone()));

    let replacement = enrollment("server-second", "environment-second", "Second", Some(true));
    writer.upsert(&replacement).await?;
    let replacement_with_preserved_preference = RemoteControlEnrollmentRecord {
        remote_control_enabled: Some(false),
        ..replacement
    };
    assert_eq!(
        load(reader).await?,
        Some(replacement_with_preserved_preference.clone())
    );

    assert_eq!(
        reader
            .set_enabled(
                WEBSOCKET_URL,
                "account-a",
                /*app_server_client_name*/ None,
                /*remote_control_enabled*/ true,
            )
            .await?,
        1
    );
    assert_eq!(
        load(writer).await?,
        Some(RemoteControlEnrollmentRecord {
            remote_control_enabled: Some(true),
            ..replacement_with_preserved_preference
        })
    );
    assert_eq!(
        reader
            .delete(
                WEBSOCKET_URL,
                "account-a",
                /*app_server_client_name*/ None
            )
            .await?,
        1
    );
    assert_eq!(load(writer).await?, None);
    Ok(())
}

#[tokio::test]
async fn sqlite_remote_control_enrollments_satisfy_shared_contract() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let writer = StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string()).await?;
    let reader = StateRuntime::init_sqlite(codex_home, "test-provider".to_string()).await?;

    run_remote_control_enrollment_contract(
        &writer.remote_control_enrollment_store(),
        &reader.remote_control_enrollment_store(),
    )
    .await?;

    writer.close().await;
    reader.close().await;
    Ok(())
}
