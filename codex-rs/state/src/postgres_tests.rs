use super::config::resolve_connection_descriptor;
use super::connection_validation::PostgresMtlsSessionEvidence;
use super::connection_validation::validate_session_evidence;
use super::*;
use pretty_assertions::assert_eq;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

fn test_config() -> PostgresNamespaceConfig {
    PostgresNamespaceConfig::new(
        "CODEX_TEST_POSTGRES_URL".to_string(),
        "codex".to_string(),
        PostgresPoolConfig::default(),
    )
    .expect("test PostgreSQL namespace config should be valid")
}

struct MtlsFileFixture {
    root: PathBuf,
}

impl MtlsFileFixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("codex-postgres-mtls-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&root).expect("mTLS fixture directory should be created");
        for name in ["root.pem", "client.pem", "client.key"] {
            fs::write(root.join(name), "test certificate material")
                .expect("mTLS fixture file should be written");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(root.join("client.key"), fs::Permissions::from_mode(0o600))
                .expect("test client key permissions should be restricted");
        }
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn tls_parameters(&self) -> Vec<(&'static str, String)> {
        vec![
            ("sslmode", "verify-full".to_string()),
            ("sslrootcert", self.path("root.pem").display().to_string()),
            ("sslcert", self.path("client.pem").display().to_string()),
            ("sslkey", self.path("client.key").display().to_string()),
        ]
    }

    fn connection_url_with_parameters(&self, base: &str, parameters: &[(&str, String)]) -> String {
        let mut url = url::Url::parse(base).expect("test PostgreSQL URL should parse");
        url.query_pairs_mut()
            .extend_pairs(parameters.iter().map(|(key, value)| (*key, value)));
        url.into()
    }

    fn connection_url(&self, host: &str) -> String {
        self.connection_url_with_parameters(
            &format!("postgresql://codex@{host}/codex"),
            &self.tls_parameters(),
        )
    }
}

impl Drop for MtlsFileFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn resolved_environment_url_builds_passwordless_mtls_connection_descriptor() {
    let files = MtlsFileFixture::new();
    let sentinel = "unique-url-sentinel.example.invalid";
    let url = files.connection_url(sentinel);
    let config = test_config();

    let descriptor = resolve_connection_descriptor(&config, |_| Some(OsString::from(url)))
        .expect("passwordless mTLS descriptor should be accepted");

    assert_eq!(
        format!("{descriptor:?}"),
        "PostgresMtlsConnectionDescriptor([REDACTED])"
    );
    assert!(!format!("{descriptor:?}").contains(sentinel));
}

#[test]
fn connection_descriptor_reports_missing_and_empty_environment_values() {
    let config = test_config();
    let missing = resolve_connection_descriptor(&config, |_| None)
        .expect_err("missing PostgreSQL URL environment variable should be rejected");
    let empty = resolve_connection_descriptor(&config, |_| Some(OsString::new()))
        .expect_err("empty PostgreSQL URL environment variable should be rejected");

    assert_eq!(
        missing.to_string(),
        "PostgreSQL URL environment variable `CODEX_TEST_POSTGRES_URL` is not set; set it to a PostgreSQL connection URL and retry"
    );
    assert_eq!(
        empty.to_string(),
        "PostgreSQL URL environment variable `CODEX_TEST_POSTGRES_URL` is empty; set it to a passwordless PostgreSQL mTLS Connection Descriptor and retry"
    );
}

#[cfg(unix)]
#[test]
fn connection_descriptor_reports_non_unicode_environment_value_without_rendering_it() {
    use std::os::unix::ffi::OsStringExt;

    let error = resolve_connection_descriptor(&test_config(), |_| {
        Some(OsString::from_vec(vec![
            b's', b'e', b'n', b't', b'i', b'n', b'e', b'l', 0xff,
        ]))
    })
    .expect_err("non-Unicode PostgreSQL URL environment variable should be rejected");

    assert_eq!(
        error.to_string(),
        "PostgreSQL URL environment variable `CODEX_TEST_POSTGRES_URL` is not valid Unicode; set it to a PostgreSQL connection URL and retry"
    );
}

#[test]
fn physical_connection_requires_tls_and_non_empty_client_certificate_dn() {
    validate_session_evidence(PostgresMtlsSessionEvidence::Present {
        tls_active: true,
        client_certificate_dn: Some("CN=codex-runtime-state".to_string()),
    })
    .expect("TLS with a client certificate DN should be accepted");

    let cases = [
        PostgresMtlsSessionEvidence::Missing,
        PostgresMtlsSessionEvidence::Present {
            tls_active: false,
            client_certificate_dn: Some("CN=codex-runtime-state".to_string()),
        },
        PostgresMtlsSessionEvidence::Present {
            tls_active: true,
            client_certificate_dn: None,
        },
        PostgresMtlsSessionEvidence::Present {
            tls_active: true,
            client_certificate_dn: Some("  ".to_string()),
        },
    ];
    for evidence in cases {
        assert_eq!(
            validate_session_evidence(evidence)
                .expect_err("incomplete mTLS session evidence should be rejected")
                .to_string(),
            "PostgreSQL physical connection did not provide required mTLS session evidence"
        );
    }
}

#[test]
fn mtls_connection_descriptor_rejects_noncanonical_url_identity() {
    let files = MtlsFileFixture::new();
    let sentinel = "unique-url-sentinel.example.invalid";
    let parameters = files.tls_parameters();
    let accepted_url = files.connection_url(sentinel);
    let with_tls = |base: &str| files.connection_url_with_parameters(base, &parameters);
    let query = accepted_url
        .split_once('?')
        .map(|(_, query)| query)
        .expect("accepted test URL should contain TLS parameters");
    let cases = [
        (
            with_tls(&format!("https://codex@{sentinel}/codex")),
            "use the `postgres` or `postgresql` scheme",
        ),
        (
            with_tls(&format!("postgresql://{sentinel}/codex")),
            "include a non-empty PostgreSQL user",
        ),
        (
            format!("postgresql://codex@/codex?{query}"),
            "include a non-empty PostgreSQL host",
        ),
        (
            with_tls(&format!("postgresql://codex@{sentinel}")),
            "include a non-empty PostgreSQL database",
        ),
        (
            with_tls(&format!("postgresql://codex:password@{sentinel}/codex")),
            "must not contain a password",
        ),
        (
            format!("{accepted_url}#connection-fragment"),
            "must not contain a fragment",
        ),
    ];

    for (url, expected_reason) in cases {
        let error = resolve_connection_descriptor(&test_config(), |_| Some(OsString::from(url)))
            .expect_err("noncanonical PostgreSQL identity should be rejected");
        let rendered = format!("{error:?} {error}");
        assert!(rendered.contains("CODEX_TEST_POSTGRES_URL"));
        assert!(rendered.contains(expected_reason), "{rendered}");
        assert!(!rendered.contains(sentinel));
        assert!(!rendered.contains("password@"));
        assert!(!rendered.contains("connection-fragment"));
    }
}

#[test]
fn mtls_connection_descriptor_requires_exactly_the_canonical_tls_parameters() {
    const EXACT_PARAMETERS: &str =
        "include exactly one each of `sslmode`, `sslrootcert`, `sslcert`, and `sslkey`";
    const CANONICAL_PARAMETERS: &str = "contain only the canonical TLS parameters";
    let files = MtlsFileFixture::new();
    let sentinel = "unique-url-sentinel.example.invalid";
    let base = format!("postgresql://codex@{sentinel}/codex");
    let valid = files.tls_parameters();
    let mut missing = valid.clone();
    missing.pop();
    let mut duplicate = valid.clone();
    duplicate.push(valid[0].clone());
    let mut alias = valid.clone();
    alias[1].0 = "ssl-root-cert";
    let mut password = valid.clone();
    password.push(("password", "must-not-be-accepted".to_string()));
    let mut wrong_mode = valid.clone();
    wrong_mode[0].1 = "verify-ca".to_string();
    let url = |parameters| files.connection_url_with_parameters(&base, parameters);
    let cases = [
        (url(&missing), EXACT_PARAMETERS),
        (url(&duplicate), EXACT_PARAMETERS),
        (url(&alias), CANONICAL_PARAMETERS),
        (url(&password), CANONICAL_PARAMETERS),
        (url(&wrong_mode), "set `sslmode` to `verify-full`"),
    ];

    for (url, expected_reason) in cases {
        let error = resolve_connection_descriptor(&test_config(), |_| Some(OsString::from(url)))
            .expect_err("noncanonical TLS parameters should be rejected");
        let rendered = format!("{error:?} {error}");
        assert!(rendered.contains("CODEX_TEST_POSTGRES_URL"));
        assert!(rendered.contains(expected_reason), "{rendered}");
        assert!(!rendered.contains(sentinel));
        assert!(!rendered.contains("must-not-be-accepted"));
    }
}

#[test]
fn mtls_connection_descriptor_requires_absolute_readable_regular_tls_files() {
    let files = MtlsFileFixture::new();
    let sentinel = "unique-url-sentinel.example.invalid";
    let base = format!("postgresql://codex@{sentinel}/codex");
    let mut relative_root = files.tls_parameters();
    relative_root[1].1 = "relative/root.pem".to_string();
    let mut empty_certificate = files.tls_parameters();
    empty_certificate[2].1.clear();
    let mut directory_key = files.tls_parameters();
    directory_key[3].1 = files.root.display().to_string();
    let mut missing_key = files.tls_parameters();
    missing_key[3].1 = files
        .path("missing-unique-url-sentinel.key")
        .display()
        .to_string();
    let cases = [
        (
            files.connection_url_with_parameters(&base, &relative_root),
            "set `sslrootcert` to a non-empty absolute path",
        ),
        (
            files.connection_url_with_parameters(&base, &empty_certificate),
            "set `sslcert` to a non-empty absolute path",
        ),
        (
            files.connection_url_with_parameters(&base, &directory_key),
            "file configured by `sslkey` must be a readable regular file",
        ),
        (
            files.connection_url_with_parameters(&base, &missing_key),
            "file configured by `sslkey` must be a readable regular file",
        ),
    ];

    for (url, expected_reason) in cases {
        let error = resolve_connection_descriptor(&test_config(), |_| Some(OsString::from(url)))
            .expect_err("invalid TLS file policy should be rejected");
        let rendered = format!("{error:?} {error}");
        assert!(rendered.contains("CODEX_TEST_POSTGRES_URL"));
        assert!(rendered.contains(expected_reason), "{rendered}");
        assert!(!rendered.contains(sentinel));
        assert!(!rendered.contains("missing-unique-url-sentinel.key"));
    }
}

#[cfg(unix)]
#[test]
fn mtls_connection_descriptor_rejects_a_fifo_without_blocking() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let files = MtlsFileFixture::new();
    let fifo = files.path("client-key.fifo");
    let fifo_path = CString::new(fifo.as_os_str().as_bytes()).expect("FIFO path has no NUL");
    // SAFETY: fifo_path is a valid NUL-terminated path owned for the duration of the call.
    assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
    let mut parameters = files.tls_parameters();
    parameters[3].1 = fifo.display().to_string();
    let url = files.connection_url_with_parameters(
        "postgresql://codex@unique-url-sentinel.example.invalid/codex",
        &parameters,
    );

    let error = resolve_connection_descriptor(&test_config(), |_| Some(url.into()))
        .expect_err("a FIFO client key should be rejected");
    assert!(error.to_string().contains("readable regular file"));
}

#[cfg(unix)]
#[test]
fn mtls_connection_descriptor_rejects_permissive_unix_key_without_changing_it() {
    use std::os::unix::fs::PermissionsExt;

    let files = MtlsFileFixture::new();
    let client_key = files.path("client.key");
    fs::set_permissions(&client_key, fs::Permissions::from_mode(0o640))
        .expect("test client key permissions should be changed");
    let url = files.connection_url("unique-url-sentinel.example.invalid");

    let error = resolve_connection_descriptor(&test_config(), |_| Some(OsString::from(url)))
        .expect_err("a group-readable client key should be rejected");

    assert!(
        error
            .to_string()
            .contains("client key must not grant any group or other permissions")
    );
    let mode = fs::metadata(client_key)
        .expect("test client key metadata should remain readable")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o640);
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
