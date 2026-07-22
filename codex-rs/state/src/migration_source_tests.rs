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
async fn preflight_rejects_each_missing_sqlite_database_without_writes() -> anyhow::Result<()> {
    let mut destination =
        PostgresContractFixture::new(test_database_url()?, "preflight_missing_db")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;
    for database in crate::runtime_db_paths(Path::new("placeholder")) {
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
async fn preflight_rejects_corrupt_sqlite_without_writes() -> anyhow::Result<()> {
    let source = test_support::initialized_source("corrupt-db").await?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    std::fs::write(crate::goals_db_path(&source), b"not a SQLite database")?;
    let mut destination = PostgresContractFixture::new(test_database_url()?, "preflight_corrupt")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;

    let error = assert_rejected_unchanged(&source, &destination).await?;

    assert!(error.contains("goals DB"), "{error}");
    assert!(error.contains("restore a healthy source backup"), "{error}");
    std::fs::write(crate::goals_db_path(&source), [])?;
    let error = assert_rejected_unchanged(&source, &destination).await?;
    assert!(error.contains("incompatible schema"), "{error}");
    destination.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn preflight_distinguishes_stale_sidecar_from_active_writer() -> anyhow::Result<()> {
    let source = test_support::initialized_source("stale-sidecar").await?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let mut sidecar = OsString::from(crate::goals_db_path(&source));
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
async fn preflight_rejects_invalid_rollout_references_without_writes() -> anyhow::Result<()> {
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
