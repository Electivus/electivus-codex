use anyhow::Result;
use codex_utils_cargo_bin::cargo_bin;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::path::Path;

#[test]
fn state_schema_uses_stable_postgresql_configuration_without_a_source_flag() -> Result<()> {
    let codex_home = tempfile::tempdir()?;
    write_direct_postgres_config(codex_home.path())?;

    let mut command = assert_cmd::Command::new(cargo_bin("codex")?);
    command
        .env("CODEX_HOME", codex_home.path())
        .args(["state", "schema", "validate"])
        .assert()
        .failure()
        .stderr(
            contains("Direct PostgreSQL URL in `state.postgresql.url`").and(contains(
                "does not contain a valid passwordless mTLS Connection Descriptor",
            )),
        );
    Ok(())
}

#[tokio::test]
async fn all_state_administration_flows_use_the_stable_postgresql_source() -> Result<()> {
    let codex_home = tempfile::tempdir()?;
    write_direct_postgres_config(codex_home.path())?;
    let sqlite_home = tempfile::tempdir()?;
    let runtime = codex_state::StateRuntime::init_sqlite(
        sqlite_home.path().to_path_buf(),
        "test-provider".to_string(),
    )
    .await?;
    runtime.close().await;
    let sqlite_home = sqlite_home.path().to_string_lossy().into_owned();

    for args in [
        vec!["state", "schema", "migrate"],
        vec!["state", "initialize"],
        vec!["state", "migrate", "--sqlite-home", &sqlite_home],
    ] {
        let mut command = assert_cmd::Command::new(cargo_bin("codex")?);
        command
            .env("CODEX_HOME", codex_home.path())
            .args(args)
            .assert()
            .failure()
            .stderr(contains(
                "Direct PostgreSQL URL in `state.postgresql.url` does not contain a valid passwordless mTLS Connection Descriptor",
            ));
    }
    Ok(())
}

#[test]
fn state_schema_accepts_a_redacted_direct_url_override() -> Result<()> {
    let direct_url = "not-a-valid-url-with-sensitive-connection-material";
    let codex_home = tempfile::tempdir()?;
    let mut command = assert_cmd::Command::new(cargo_bin("codex")?);
    command
        .env("CODEX_HOME", codex_home.path())
        .args(["state", "schema", "validate", "--url", direct_url])
        .assert()
        .failure()
        .stderr(
            contains("Direct PostgreSQL URL supplied with `--url`")
                .and(contains(
                    "does not contain a valid passwordless mTLS Connection Descriptor",
                ))
                .and(contains(direct_url).not())
                .and(contains("sensitive-connection-material").not()),
        );
    Ok(())
}

#[test]
fn state_schema_rejects_ambiguous_source_overrides_without_exposing_the_url() -> Result<()> {
    let direct_url = "sensitive-direct-url-material";
    let mut command = assert_cmd::Command::new(cargo_bin("codex")?);
    command
        .args([
            "state",
            "schema",
            "validate",
            "--url",
            direct_url,
            "--url-env",
            "CODEX_TEST_POSTGRES_URL",
        ])
        .assert()
        .failure()
        .stderr(
            contains("--url")
                .and(contains("--url-env"))
                .and(contains(direct_url).not()),
        );
    Ok(())
}

#[test]
fn state_schema_requires_stable_postgresql_or_an_explicit_source() -> Result<()> {
    let codex_home = tempfile::tempdir()?;
    let mut command = assert_cmd::Command::new(cargo_bin("codex")?);
    command
        .env("CODEX_HOME", codex_home.path())
        .args(["state", "schema", "validate"])
        .assert()
        .failure()
        .stderr(
            contains("PostgreSQL Runtime State is not configured")
                .and(contains("state.backend = \"postgresql\""))
                .and(contains("--url <URL>"))
                .and(contains("--url-env <ENV_VAR>")),
        );
    Ok(())
}

#[test]
fn state_schema_help_describes_both_explicit_source_choices() -> Result<()> {
    let mut command = assert_cmd::Command::new(cargo_bin("codex")?);
    command
        .args(["state", "schema", "validate", "--help"])
        .assert()
        .success()
        .stdout(
            contains("--url <URL>")
                .and(contains("Direct passwordless PostgreSQL URL"))
                .and(contains("--url-env <ENV_VAR>"))
                .and(contains(
                    "Environment variable containing a passwordless PostgreSQL URL",
                )),
        );
    Ok(())
}

#[test]
fn state_schema_rejects_cli_namespace_overrides_without_a_source_override() -> Result<()> {
    let codex_home = tempfile::tempdir()?;
    let mut command = assert_cmd::Command::new(cargo_bin("codex")?);
    command
        .env("CODEX_HOME", codex_home.path())
        .args([
            "state",
            "schema",
            "validate",
            "--schema",
            "ambiguous_namespace",
        ])
        .assert()
        .failure()
        .stderr(
            contains("required arguments")
                .and(contains("--schema"))
                .and(contains("--url"))
                .and(contains("--url-env")),
        );
    Ok(())
}

#[test]
fn state_schema_validate_requires_the_referenced_environment_variable() -> Result<()> {
    let mut command = assert_cmd::Command::new(cargo_bin("codex")?);
    command
        .env_remove("CODEX_TEST_POSTGRES_URL")
        .args([
            "state",
            "schema",
            "validate",
            "--url-env",
            "CODEX_TEST_POSTGRES_URL",
        ])
        .assert()
        .failure()
        .stderr(
            contains("PostgreSQL URL environment variable `CODEX_TEST_POSTGRES_URL` is not set")
                .and(contains("deprecat").not()),
        );
    Ok(())
}

#[test]
fn state_schema_errors_never_print_the_connection_url() -> Result<()> {
    let secret_url = "not-a-url-with-super-secret-password";
    let mut command = assert_cmd::Command::new(cargo_bin("codex")?);
    command
        .env("CODEX_TEST_POSTGRES_URL", secret_url)
        .args([
            "state",
            "schema",
            "migrate",
            "--url-env",
            "CODEX_TEST_POSTGRES_URL",
        ])
        .assert()
        .failure()
        .stderr(
            contains("does not contain a valid passwordless mTLS Connection Descriptor")
                .and(contains(secret_url).not())
                .and(contains("super-secret-password").not()),
        );
    Ok(())
}

fn write_direct_postgres_config(codex_home: &Path) -> Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        r#"
[features]
postgresql_state = true

[state]
backend = "postgresql"

[state.postgresql]
url = "not-a-valid-postgresql-descriptor"
schema = "stable_namespace"
"#,
    )?;
    Ok(())
}
