use super::preflight_runtime_state_migration;
use super::test_support;
use crate::PostgresNamespaceAction;
use crate::SqliteConfig;
use crate::postgres::test_support::PostgresContractFixture;
use crate::postgres::test_support::test_database_url;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::ffi::OsString;
use std::path::Path;

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_preflight_rejects_each_missing_sqlite_database_without_writes()
-> anyhow::Result<()> {
    let mut destination =
        PostgresContractFixture::new(test_database_url()?, "preflight_missing_db")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(
        std::env::current_dir()?.join("placeholder"),
    )?);
    for database in sqlite.runtime_db_paths() {
        let source = test_support::initialized_source("missing-db").await?;
        let database_path = source.join(
            database
                .path
                .file_name()
                .expect("runtime database path has a filename"),
        );
        std::fs::remove_file(database_path)?;
        let error = assert_rejected_unchanged(&source, &destination).await?;
        assert!(error.contains(database.label), "{error}");
        assert!(error.contains("is missing"), "{error}");
        std::fs::remove_dir_all(source)?;
    }
    destination.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_preflight_rejects_corrupt_sqlite_without_writes() -> anyhow::Result<()> {
    let source = test_support::initialized_source("corrupt-db").await?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?);
    std::fs::write(sqlite.goals_db_path(), b"not a SQLite database")?;
    let mut destination = PostgresContractFixture::new(test_database_url()?, "preflight_corrupt")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;

    let error = assert_rejected_unchanged(&source, &destination).await?;

    assert!(error.contains("goals DB"), "{error}");
    assert!(error.contains("restore a healthy source backup"), "{error}");
    std::fs::write(sqlite.goals_db_path(), [])?;
    let error = assert_rejected_unchanged(&source, &destination).await?;
    assert!(error.contains("incompatible schema"), "{error}");
    destination.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_preflight_distinguishes_stale_sidecar_from_active_writer()
-> anyhow::Result<()> {
    let source = test_support::initialized_source("stale-sidecar").await?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?);
    let mut sidecar = OsString::from(sqlite.goals_db_path());
    sidecar.push("-journal");
    std::fs::write(sidecar, b"stale")?;
    let mut destination = PostgresContractFixture::new(test_database_url()?, "preflight_sidecar")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;

    let error = assert_rejected_unchanged(&source, &destination).await?;

    assert!(error.contains("uncheckpointed sidecar"), "{error}");
    assert!(error.contains("recover or checkpoint"), "{error}");
    assert!(!error.contains("active SQLite writer"), "{error}");
    destination.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_preflight_rejects_invalid_rollout_references_without_writes()
-> anyhow::Result<()> {
    let mut destination = PostgresContractFixture::new(test_database_url()?, "preflight_rollouts")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;

    let missing = test_support::source_with_rollout("rollout-missing", |source| {
        source.join("sessions/missing.jsonl")
    })
    .await?;
    let error = assert_rejected_unchanged(&missing, &destination).await?;
    assert!(
        error.contains("references missing rollout JSONL"),
        "{error}"
    );
    std::fs::remove_dir_all(missing)?;

    let outside = test_support::source_with_rollout("rollout-outside", |_| {
        std::path::PathBuf::from("../outside-codex-home.jsonl")
    })
    .await?;
    let error = assert_rejected_unchanged(&outside, &destination).await?;
    assert!(error.contains("outside the source home"), "{error}");
    std::fs::remove_dir_all(outside)?;

    let non_jsonl = test_support::source_with_rollout("rollout-extension", |source| {
        source.join("sessions/rollout.txt")
    })
    .await?;
    std::fs::create_dir_all(non_jsonl.join("sessions"))?;
    std::fs::write(non_jsonl.join("sessions/rollout.txt"), b"{}\n")?;
    let error = assert_rejected_unchanged(&non_jsonl, &destination).await?;
    assert!(error.contains("references non-JSONL rollout"), "{error}");
    std::fs::remove_dir_all(non_jsonl)?;

    let invalid = test_support::source_with_rollout("rollout-invalid", |source| {
        source.join("sessions/invalid.jsonl")
    })
    .await?;
    std::fs::create_dir_all(invalid.join("sessions"))?;
    std::fs::write(invalid.join("sessions/invalid.jsonl"), b"{not-json}\n")?;
    let error = assert_rejected_unchanged(&invalid, &destination).await?;
    assert!(error.contains("contains invalid JSON at line 1"), "{error}");
    std::fs::remove_dir_all(invalid)?;

    let ambiguous = test_support::source_with_rollout("rollout-ambiguous", |source| {
        source.join("sessions/ambiguous.jsonl")
    })
    .await?;
    std::fs::create_dir_all(ambiguous.join("sessions"))?;
    for extension in ["jsonl", "jsonl.zst"] {
        std::fs::write(
            ambiguous.join(format!("sessions/ambiguous.{extension}")),
            b"",
        )?;
    }
    let error = assert_rejected_unchanged(&ambiguous, &destination).await?;
    assert!(
        error.contains("ambiguous physical rollout files"),
        "{error}"
    );
    std::fs::remove_dir_all(ambiguous)?;

    destination.cleanup().await?;
    Ok(())
}

async fn assert_rejected_unchanged(
    source: &Path,
    destination: &PostgresContractFixture,
) -> anyhow::Result<String> {
    let source_before = test_support::snapshot_source(source)?;
    let destination_before = test_support::snapshot_destination(destination).await?;
    let error = preflight_runtime_state_migration(
        SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source)?),
        destination.config_for_tests(),
    )
    .await
    .expect_err("invalid migration source must be rejected");
    assert_eq!(test_support::snapshot_source(source)?, source_before);
    assert_eq!(
        test_support::snapshot_destination(destination).await?,
        destination_before
    );
    Ok(format!("{error:#}"))
}
