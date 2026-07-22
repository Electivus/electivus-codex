use super::preflight_runtime_state_migration;
use crate::PostgresNamespaceAction;
use crate::SqliteConfig;
use crate::open_thread_history_db;
use crate::postgres::test_support::PostgresContractFixture;
use crate::postgres::test_support::test_database_url;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::time::SystemTime;

#[tokio::test]
async fn preflight_inventories_all_source_authorities() -> anyhow::Result<()> {
    let source = std::env::temp_dir().join(format!(
        "codex-migration-preflight-{}-{}",
        std::process::id(),
        SystemTime::UNIX_EPOCH.elapsed()?.as_nanos()
    ));
    std::fs::create_dir(&source)?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let runtime = crate::StateRuntime::init(source.clone(), "test-provider".to_string()).await?;
    let history = open_thread_history_db(&source).await?;
    history.close().await;
    runtime.close().await;

    tokio::fs::create_dir_all(source.join("sessions/2026/07/22")).await?;
    tokio::fs::write(
        source.join("sessions/2026/07/22/rollout.jsonl"),
        b"{\"type\":\"session_meta\"}\n",
    )
    .await?;
    tokio::fs::create_dir_all(
        source.join("memories/extensions/external_agent_import/resources/project-a"),
    )
    .await?;
    tokio::fs::write(source.join("memories/MEMORY.md"), b"# Memory\n").await?;
    tokio::fs::write(
        source.join("memories/extensions/external_agent_import/resources/project-a/imported.md"),
        b"# Imported\n",
    )
    .await?;

    let mut destination =
        PostgresContractFixture::new(test_database_url()?, "preflight_inventory")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;
    let inventory = preflight_runtime_state_migration(
        SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?),
        destination.config_for_tests(),
    )
    .await?;

    assert_eq!(inventory.databases().len(), 5);
    assert_eq!(
        inventory
            .rollout_files()
            .iter()
            .map(super::SourceFileInventory::relative_path)
            .collect::<Vec<_>>(),
        vec![std::path::Path::new("sessions/2026/07/22/rollout.jsonl")]
    );
    assert_eq!(
        inventory
            .memory_files()
            .iter()
            .map(super::SourceFileInventory::relative_path)
            .collect::<Vec<_>>(),
        vec![std::path::Path::new("memories/MEMORY.md")]
    );
    assert_eq!(
        inventory
            .imported_resources()
            .iter()
            .map(super::SourceFileInventory::relative_path)
            .collect::<Vec<_>>(),
        vec![std::path::Path::new(
            "memories/extensions/external_agent_import/resources/project-a/imported.md"
        )]
    );

    destination.cleanup().await?;
    Ok(())
}
