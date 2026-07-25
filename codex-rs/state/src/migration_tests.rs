use super::preflight_runtime_state_migration;
use crate::PostgresNamespaceAction;
use crate::SqliteConfig;
use crate::open_thread_history_db;
use crate::postgres::test_support::PostgresContractFixture;
use crate::postgres::test_support::test_database_url;
use anyhow::Context;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use sqlx::Connection;
use std::process::Command;
use std::time::Duration;
use std::time::SystemTime;

use super::test_support;

#[tokio::test]
async fn rollout_validation_bounds_plain_and_compressed_decoded_lines() -> anyhow::Result<()> {
    let source = test_support::initialized_source("rollout-validation-budget").await?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    for (name, contents) in [
        ("oversized.jsonl", b"{}\n".to_vec()),
        (
            "oversized.jsonl.zst",
            zstd::stream::encode_all(b"{}\n".as_slice(), /*level*/ 0)?,
        ),
    ] {
        std::fs::write(source.join(name), contents)?;
        super::source_validation::validate_json_lines(
            &source,
            std::path::Path::new(name),
            /*maximum_bytes*/ 1,
        )
        .await
        .expect_err("decoded line must respect the validation budget");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_preflight_inventories_all_source_authorities() -> anyhow::Result<()> {
    let source = std::env::temp_dir().join(format!(
        "codex-migration-preflight-{}-{}",
        std::process::id(),
        SystemTime::UNIX_EPOCH.elapsed()?.as_nanos()
    ));
    std::fs::create_dir(&source)?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let runtime =
        crate::StateRuntime::init_sqlite(source.clone(), "test-provider".to_string()).await?;
    let history = open_thread_history_db(runtime.sqlite()).await?;
    history.close().await;
    runtime.close().await;
    tokio::fs::create_dir_all(source.join("sessions/2026/07/22")).await?;
    tokio::fs::write(
        source.join("sessions/2026/07/22/rollout.jsonl"),
        b"{\"type\":\"session_meta\"}\n",
    )
    .await?;
    tokio::fs::write(
        source.join("sessions/2026/07/22/rollout-only.jsonl.zst"),
        zstd::stream::encode_all(&b"{\"type\":\"session_meta\"}\n"[..], /*level*/ 0)?,
    )
    .await?;
    tokio::fs::write(
        source.join("session_index.jsonl"),
        b"{\"id\":\"019c84d0-2222-7777-8222-222222222222\",\"thread_name\":\"Indexed\",\"updated_at\":\"2026-07-22T10:00:00Z\"}\n",
    )
    .await?;
    tokio::fs::create_dir_all(
        source.join("memories/extensions/external_agent_import/resources/project-a"),
    )
    .await?;
    tokio::fs::write(source.join("memories/MEMORY.md"), b"# Memory\n").await?;
    tokio::fs::write(source.join("config.toml"), b"model = \"gpt-5\"\n").await?;
    tokio::fs::create_dir_all(source.join("memories/extensions/external_agent_import")).await?;
    tokio::fs::write(
        source.join("memories/extensions/external_agent_import/instructions.md"),
        b"# Import instructions\n",
    )
    .await?;
    tokio::fs::write(
        source.join("memories/extensions/external_agent_import/resources/project-a/scope.json"),
        b"{\"roots\":[]}\n",
    )
    .await?;
    tokio::fs::write(
        source.join("memories/extensions/external_agent_import/resources/project-a/imported.md"),
        b"# Imported\n",
    )
    .await?;

    let mut destination =
        PostgresContractFixture::new(test_database_url()?, "preflight_inventory")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;
    let source_before = test_support::snapshot_source(&source)?;
    let destination_before = test_support::snapshot_destination(&destination).await?;
    let inventory = preflight_runtime_state_migration(
        SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?),
        destination.config_for_tests(),
    )
    .await?;
    let source_after = test_support::snapshot_source(&source)?;
    let destination_after = test_support::snapshot_destination(&destination).await?;

    assert_eq!(source_after, source_before);
    assert_eq!(destination_after, destination_before);

    assert_eq!(inventory.databases().len(), 5);
    assert_eq!(
        inventory
            .rollout_files()
            .iter()
            .map(super::SourceFileInventory::relative_path)
            .collect::<Vec<_>>(),
        vec![
            std::path::Path::new("sessions/2026/07/22/rollout-only.jsonl.zst"),
            std::path::Path::new("sessions/2026/07/22/rollout.jsonl"),
        ]
    );
    assert_eq!(
        inventory
            .session_index()
            .map(super::SourceFileInventory::relative_path),
        Some(std::path::Path::new("session_index.jsonl"))
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
        vec![
            std::path::Path::new("memories/extensions/external_agent_import/instructions.md"),
            std::path::Path::new(
                "memories/extensions/external_agent_import/resources/project-a/imported.md"
            ),
            std::path::Path::new(
                "memories/extensions/external_agent_import/resources/project-a/scope.json"
            ),
        ]
    );

    destination.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_preflight_reports_a_positive_active_writer_check() -> anyhow::Result<()>
{
    let source = std::env::temp_dir().join(format!(
        "codex-migration-active-writer-{}-{}",
        std::process::id(),
        SystemTime::UNIX_EPOCH.elapsed()?.as_nanos()
    ));
    std::fs::create_dir(&source)?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let runtime =
        crate::StateRuntime::init_sqlite(source.clone(), "test-provider".to_string()).await?;
    let history = open_thread_history_db(runtime.sqlite()).await?;
    history.close().await;
    runtime.close().await;
    std::fs::write(source.join("config.toml"), b"model = \"gpt-5\"\n")?;

    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?);
    let ready_path = source.join("writer-ready");
    let release_path = source.join("writer-release");
    let writer = Command::new(std::env::current_exe()?)
        .arg("--ignored")
        .arg("--exact")
        .arg("migration::tests::sqlite_writer_process_fixture")
        .arg("--nocapture")
        .env("CODEX_MIGRATION_WRITER_DB", sqlite.state_db_path())
        .env("CODEX_MIGRATION_WRITER_READY", &ready_path)
        .env("CODEX_MIGRATION_WRITER_RELEASE", &release_path)
        .spawn()?;
    let writer_guard = scopeguard::guard((writer, release_path), |(mut writer, release_path)| {
        let _ = std::fs::write(release_path, []);
        let _ = writer.wait();
    });
    for _ in 0..100 {
        if ready_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::ensure!(
        ready_path.exists(),
        "SQLite writer fixture did not become ready"
    );

    let mut destination = PostgresContractFixture::new(test_database_url()?, "preflight_writer")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;
    let source_before = test_support::snapshot_source(&source)?;
    let destination_before = test_support::snapshot_destination(&destination).await?;
    let error = preflight_runtime_state_migration(sqlite, destination.config_for_tests())
        .await
        .expect_err("an active SQLite writer must block migration preflight");

    assert!(
        error.to_string().contains("active SQLite writer"),
        "{error:#}"
    );
    assert_eq!(test_support::snapshot_source(&source)?, source_before);
    assert_eq!(
        test_support::snapshot_destination(&destination).await?,
        destination_before
    );
    drop(writer_guard);
    destination.cleanup().await?;
    Ok(())
}

#[test]
#[ignore = "helper process for active SQLite writer coverage"]
fn sqlite_writer_process_fixture() -> anyhow::Result<()> {
    let database_path = std::env::var_os("CODEX_MIGRATION_WRITER_DB")
        .map(std::path::PathBuf::from)
        .context("CODEX_MIGRATION_WRITER_DB is missing")?;
    let ready_path = std::env::var_os("CODEX_MIGRATION_WRITER_READY")
        .map(std::path::PathBuf::from)
        .context("CODEX_MIGRATION_WRITER_READY is missing")?;
    let release_path = std::env::var_os("CODEX_MIGRATION_WRITER_RELEASE")
        .map(std::path::PathBuf::from)
        .context("CODEX_MIGRATION_WRITER_RELEASE is missing")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let sqlite_home = database_path
            .parent()
            .context("writer fixture database has no parent")?;
        let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(sqlite_home)?);
        let pool = sqlite.open_read_write_pool(&database_path).await?;
        let mut connection = pool.acquire().await?;
        let mut transaction = connection.begin().await?;
        sqlx::query("UPDATE backfill_state SET status = status WHERE id = 1")
            .execute(&mut *transaction)
            .await?;
        std::fs::write(&ready_path, [])?;
        while !release_path.exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        transaction.rollback().await?;
        drop(connection);
        pool.close().await;
        anyhow::Ok(())
    })
}
