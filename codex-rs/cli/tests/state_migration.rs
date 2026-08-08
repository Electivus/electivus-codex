use anyhow::Context;
use anyhow::Result;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cargo_bin::cargo_bin;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;

const DATABASE_URL_ENV: &str = "CODEX_CLI_MIGRATION_TEST_URL";

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn state_migrate_process_runs_the_complete_offline_migration() -> Result<()> {
    let database_url = std::env::var("CODEX_TEST_POSTGRES_URL")
        .context("CODEX_TEST_POSTGRES_URL must point to PostgreSQL 18")?;
    let source = tempfile::tempdir()?;
    let runtime = codex_state::StateRuntime::init_sqlite(
        source.path().to_path_buf(),
        "test-provider".to_string(),
    )
    .await?;
    let sqlite =
        codex_state::SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.path())?);
    runtime.close().await;
    let runtime_db_paths = sqlite.runtime_db_paths();
    anyhow::ensure!(
        runtime_db_paths.len() == 5
            && runtime_db_paths
                .iter()
                .filter(|database| database.path != sqlite.thread_history_db_path())
                .all(|database| database.path.is_file())
            && !sqlite.thread_history_db_path().exists(),
        "process fixture must omit only the optional thread history database"
    );
    let config = b"model = \"gpt-5\"\n";
    let memory = b"# Preserved process migration memory\n";
    std::fs::write(source.path().join("config.toml"), config)?;
    std::fs::create_dir(source.path().join("memories"))?;
    std::fs::write(source.path().join("memories/MEMORY.md"), memory)?;
    let source_before = snapshot_source(source.path())?;

    let schema = format!(
        "cli_state_migration_{}_{}",
        std::process::id(),
        SystemTime::UNIX_EPOCH.elapsed()?.as_nanos()
    );
    let pool = sqlx::PgPool::connect(&database_url).await?;
    let test_result = async {
        let codex = cargo_bin("codex")?;
        let setup = Command::new(&codex)
            .env(DATABASE_URL_ENV, &database_url)
            .args([
                "state",
                "schema",
                "migrate",
                "--url-env",
                DATABASE_URL_ENV,
                "--schema",
                &schema,
            ])
            .output()?;
        anyhow::ensure!(
            setup.status.success(),
            "schema setup process failed: stdout={} stderr={}",
            String::from_utf8_lossy(&setup.stdout),
            String::from_utf8_lossy(&setup.stderr)
        );

        let migration = Command::new(codex)
            .env(DATABASE_URL_ENV, &database_url)
            .args([
                "state",
                "migrate",
                "--sqlite-home",
                source.path().to_str().context("non-UTF-8 SQLite home")?,
                "--url-env",
                DATABASE_URL_ENV,
                "--schema",
                &schema,
            ])
            .output()?;
        anyhow::ensure!(
            migration.status.success(),
            "Runtime State Migration process failed: stdout={} stderr={}",
            String::from_utf8_lossy(&migration.stdout),
            String::from_utf8_lossy(&migration.stderr)
        );
        let stdout = String::from_utf8(migration.stdout)?;
        for expected in [
            format!("PostgreSQL Runtime State Namespace `{schema}` is READY at migration fence 4."),
            "config.toml was not changed; select the PostgreSQL backend separately after review."
                .to_string(),
            "WARNING: This migration is forward-only.".to_string(),
            "the preserved SQLite source becomes stale".to_string(),
        ] {
            anyhow::ensure!(
                stdout.contains(&expected),
                "migration report omitted `{expected}`: {stdout}"
            );
        }
        let migration_table = format!("\"{schema}\".runtime_state_migration");
        let progress: (String, bool, i64) = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT phase, ready, fencing_token FROM {migration_table} WHERE singleton"
        )))
        .fetch_one(&pool)
        .await?;
        anyhow::ensure!(
            progress == ("ready".to_string(), true, 4),
            "CLI did not complete the full migration phase chain: {progress:?}"
        );
        snapshot_source(source.path())
    }
    .await;
    let cleanup_result = sqlx::query(AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"
    )))
    .execute(&pool)
    .await;
    pool.close().await;
    let source_after = test_result?;
    cleanup_result?;
    assert_eq!(source_after, source_before);
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct SourceFileSnapshot {
    relative_path: PathBuf,
    bytes: Vec<u8>,
    modified_at: SystemTime,
}

fn snapshot_source(source: &Path) -> Result<Vec<SourceFileSnapshot>> {
    let mut pending = vec![source.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                anyhow::ensure!(
                    metadata.is_file(),
                    "unexpected source entry: {}",
                    entry.path().display()
                );
                files.push(SourceFileSnapshot {
                    relative_path: entry.path().strip_prefix(source)?.to_path_buf(),
                    bytes: std::fs::read(entry.path())?,
                    modified_at: metadata.modified()?,
                });
            }
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}
