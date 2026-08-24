use anyhow::Result;
use codex_utils_cargo_bin::cargo_bin;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

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
        .stderr(contains(
            "PostgreSQL URL environment variable `CODEX_TEST_POSTGRES_URL` is not set",
        ));
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
