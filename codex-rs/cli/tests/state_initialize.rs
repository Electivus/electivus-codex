#![allow(
    clippy::disallowed_methods,
    reason = "PostgreSQL tests connect only to PostgreSQL pools"
)]

use anyhow::Context;
use anyhow::Result;
use codex_state::PostgresNamespaceConfig;
use codex_state::PostgresPoolConfig;
use codex_state::PostgresRuntimeStatePool;
use codex_utils_cargo_bin::cargo_bin;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;
use std::process::Command;
use std::time::SystemTime;

const DATABASE_URL_ENV: &str = "CODEX_TEST_POSTGRES_URL";

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn state_initialize_process_creates_empty_runtime_ready_postgres_without_sqlite() -> Result<()>
{
    let database_url = std::env::var(DATABASE_URL_ENV)
        .with_context(|| format!("{DATABASE_URL_ENV} must point to PostgreSQL 18"))?;
    let schema = format!(
        "cli_state_initialize_{}_{}",
        std::process::id(),
        SystemTime::UNIX_EPOCH.elapsed()?.as_nanos()
    );
    let isolated_home = tempfile::tempdir()?;
    let sqlite_home = isolated_home.path().join("must-not-be-created");
    let pool = sqlx::PgPool::connect(&database_url).await?;

    let test_result = async {
        let initialization = Command::new(cargo_bin("codex")?)
            .env(codex_state::SQLITE_HOME_ENV, &sqlite_home)
            .args([
                "state",
                "initialize",
                "--url-env",
                DATABASE_URL_ENV,
                "--schema",
                &schema,
            ])
            .output()?;
        anyhow::ensure!(
            initialization.status.success(),
            "empty initialization process failed: stdout={} stderr={}",
            String::from_utf8_lossy(&initialization.stdout),
            String::from_utf8_lossy(&initialization.stderr)
        );
        let stdout = String::from_utf8(initialization.stdout)?;
        for expected in [
            format!(
                "PostgreSQL Runtime State Namespace `{schema}` was initialized empty and is READY at readiness fence 1."
            ),
            "No SQLite Runtime State Namespace was read or migrated.".to_string(),
            "config.toml was not changed; select the PostgreSQL backend separately after review."
                .to_string(),
        ] {
            anyhow::ensure!(
                stdout.contains(&expected),
                "initialization report omitted `{expected}`: {stdout}"
            );
        }
        anyhow::ensure!(
            !sqlite_home.exists(),
            "empty PostgreSQL initialization accessed SQLite home"
        );

        let namespace = PostgresNamespaceConfig::new(
            DATABASE_URL_ENV.to_string(),
            schema.clone(),
            PostgresPoolConfig::default(),
        )?;
        let runtime_pool = PostgresRuntimeStatePool::connect(namespace).await?;
        let generation = runtime_pool
            .memory_store()
            .load_active_memory_generation()
            .await?
            .context("empty initialization must publish an active Memory Generation")?;
        assert_eq!(generation.completed_watermark(), 0);
        assert_eq!(generation.artifacts(), []);
        runtime_pool.close().await;
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let cleanup_result = sqlx::query(AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"
    )))
    .execute(&pool)
    .await;
    pool.close().await;
    test_result?;
    cleanup_result?;
    Ok(())
}
