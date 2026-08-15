use super::preflight_runtime_state_migration;
use crate::PostgresNamespaceConfig;
use crate::PostgresPoolConfig;
use crate::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::num::NonZeroU32;
use std::process::Command;
use std::time::Duration;

const REDACTION_URL_ENV: &str = "CODEX_MIGRATION_REDACTION_URL";

#[test]
fn preflight_connection_failures_redact_representative_credentials() -> anyhow::Result<()> {
    let urls = "postgresql://codex:p%40ss%2Fword@127.0.0.1:1/codex?application_name=query-marker;postgresql://127.0.0.1:1/codex?user=codex&password=query%3Asecret&application_name=query-marker;postgresql://codex@127.0.0.1:1/codex?unrecognized=parameter-secret";
    for url in urls.split(';') {
        let output = Command::new(std::env::current_exe()?)
            .arg("--ignored")
            .arg("--exact")
            .arg("migration::redaction_tests::postgres_redaction_process_fixture")
            .arg("--nocapture")
            .env(REDACTION_URL_ENV, url)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "redaction child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[test]
#[ignore = "helper process for PostgreSQL credential redaction coverage"]
fn postgres_redaction_process_fixture() -> anyhow::Result<()> {
    let connection_url = std::env::var(REDACTION_URL_ENV)?;
    let source = crate::runtime::test_support::unique_temp_dir();
    std::fs::create_dir(&source)?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let error = runtime
        .block_on(preflight_runtime_state_migration(
            SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?),
            PostgresNamespaceConfig::new(
                REDACTION_URL_ENV.to_string(),
                "redaction_fixture".to_string(),
                PostgresPoolConfig::new(
                    /*max_connections*/ NonZeroU32::MIN,
                    /*acquire_timeout*/ Duration::from_secs(1),
                    /*statement_timeout*/ Duration::from_secs(1),
                )?,
            )?,
        ))
        .expect_err("unreachable or invalid PostgreSQL URL must fail preflight");
    let rendered = format!("{error:?} {error:#}");
    for secret in [
        connection_url.as_str(),
        "p%40ss%2Fword",
        "p@ss/word",
        "query%3Asecret",
        "query:secret",
        "query-marker",
        "parameter-secret",
    ] {
        assert!(
            !rendered.contains(secret),
            "leaked `{secret}` in {rendered}"
        );
    }
    Ok(())
}
