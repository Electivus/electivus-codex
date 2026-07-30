use super::config::ResolvedUrl;
use super::config::parse_connection_options;
use super::config::resolve_url;
use super::*;
use pretty_assertions::assert_eq;
use std::ffi::OsString;

fn test_config() -> PostgresNamespaceConfig {
    PostgresNamespaceConfig::new(
        "CODEX_TEST_POSTGRES_URL".to_string(),
        "codex".to_string(),
        PostgresPoolConfig::default(),
    )
    .expect("test PostgreSQL namespace config should be valid")
}

#[test]
fn resolved_url_debug_output_is_redacted() {
    let secret = "postgresql://codex:super-secret@example.invalid/codex";
    let config = test_config();
    let resolved = resolve_url(&config, |_| Some(OsString::from(secret)))
        .expect("test connection URL should resolve");

    assert_eq!(format!("{resolved:?}"), "ResolvedUrl([REDACTED])");
}

#[test]
fn invalid_connection_url_error_does_not_include_secret_value() {
    let config = test_config();
    for secret in ["super-secret-value", "query-parameter-secret"] {
        let value = match secret {
            "super-secret-value" => secret.to_string(),
            _ => format!("postgresql://codex@example.invalid/codex?unrecognized={secret}"),
        };
        let resolved = ResolvedUrl(value);
        let error = parse_connection_options(&config, &resolved)
            .expect_err("invalid PostgreSQL URL should be rejected");
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(secret));
        assert!(error.to_string().contains("does not contain a valid"));
    }
}

#[test]
fn namespace_config_rejects_a_literal_url_in_place_of_an_environment_variable_name() {
    let secret = "postgresql://codex:super-secret@example.invalid/codex";
    let error = PostgresNamespaceConfig::new(
        secret.to_string(),
        "codex".to_string(),
        PostgresPoolConfig::default(),
    )
    .expect_err("a connection URL must not be retained as configuration");

    assert!(!error.to_string().contains(secret));
    assert!(error.to_string().contains("environment-variable reference"));
}

#[test]
fn migration_history_requires_a_contiguous_sequence_starting_at_one() {
    let schema = "isolated_namespace";
    let valid = MigrationHistory {
        minimum: Some(1),
        maximum: Some(3),
        count: 3,
    };
    let missing_version = MigrationHistory {
        minimum: Some(1),
        maximum: Some(3),
        count: 2,
    };

    let version = valid
        .current_version(schema)
        .expect("contiguous migration history should be valid");
    assert_eq!(version, Some(3));
    assert_eq!(
        missing_version
            .current_version(schema)
            .expect_err("migration gaps should be rejected")
            .to_string(),
        "PostgreSQL schema `isolated_namespace` has an invalid Codex migration history; restore it or provision a new Runtime State Namespace"
    );
}

#[test]
fn schema_versions_outside_the_compatibility_range_are_actionable() {
    let schema = "isolated_namespace";
    let newer_version = MAXIMUM_COMPATIBLE_SCHEMA_VERSION + 1;

    assert_eq!(
        ensure_compatible_schema_version(schema, /*version*/ 0)
            .expect_err("older schema should be rejected")
            .to_string(),
        "PostgreSQL schema `isolated_namespace` is at version 0, older than the minimum supported version 1; run a compatible Codex schema migration command"
    );
    assert_eq!(
        ensure_compatible_schema_version(schema, newer_version)
            .expect_err("newer schema should be rejected")
            .to_string(),
        format!(
            "PostgreSQL schema `isolated_namespace` is at version {newer_version}, newer than the maximum supported version {MAXIMUM_COMPATIBLE_SCHEMA_VERSION}; upgrade Codex before using this namespace"
        )
    );
}

#[test]
fn postgresql_versions_before_18_are_rejected() {
    assert_eq!(
        ensure_supported_postgres_version(/*detected_major*/ 17)
            .expect_err("PostgreSQL 17 should be rejected")
            .to_string(),
        "PostgreSQL 17 is unsupported; PostgreSQL 18 or later is required"
    );
    ensure_supported_postgres_version(/*detected_major*/ 18)
        .expect("PostgreSQL 18 should be supported");
}
