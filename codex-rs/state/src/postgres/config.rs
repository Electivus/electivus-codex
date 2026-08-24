use super::connection_descriptor::PostgresMtlsConnectionDescriptor;
use super::connection_validation::validate_physical_connection;
use anyhow::anyhow;
use log::LevelFilter;
use sqlx::ConnectOptions;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::ffi::OsString;
use std::num::NonZeroU32;
use std::time::Duration;

/// Connection-pool limits used by PostgreSQL Runtime State Namespace operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostgresPoolConfig {
    max_connections: NonZeroU32,
    acquire_timeout: Duration,
    statement_timeout: Duration,
}

impl PostgresPoolConfig {
    pub fn new(
        max_connections: NonZeroU32,
        acquire_timeout: Duration,
        statement_timeout: Duration,
    ) -> anyhow::Result<Self> {
        if acquire_timeout.is_zero() {
            anyhow::bail!("PostgreSQL acquire timeout must be greater than zero");
        }
        if statement_timeout.is_zero() {
            anyhow::bail!("PostgreSQL statement timeout must be greater than zero");
        }
        Ok(Self {
            max_connections,
            acquire_timeout,
            statement_timeout,
        })
    }
}

impl Default for PostgresPoolConfig {
    fn default() -> Self {
        Self {
            max_connections: NonZeroU32::new(10).unwrap_or(NonZeroU32::MIN),
            acquire_timeout: Duration::from_secs(10),
            statement_timeout: Duration::from_secs(30),
        }
    }
}

/// Non-secret configuration identifying one PostgreSQL Runtime State Namespace.
///
/// `url_env` is the name of the environment variable containing the connection
/// URL. The URL itself is resolved only while performing an operation and is
/// never retained in this configuration or included in its debug output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostgresNamespaceConfig {
    pub(super) url_env: String,
    pub(super) schema: String,
    pool: PostgresPoolConfig,
}

impl PostgresNamespaceConfig {
    pub fn new(url_env: String, schema: String, pool: PostgresPoolConfig) -> anyhow::Result<Self> {
        validate_url_environment_variable(&url_env)?;
        validate_schema_name(&schema)?;
        Ok(Self {
            url_env,
            schema,
            pool,
        })
    }

    /// Returns the non-secret namespace schema name.
    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub(crate) fn url_env(&self) -> &str {
        &self.url_env
    }
}

pub(crate) async fn connect_pool(config: &PostgresNamespaceConfig) -> anyhow::Result<PgPool> {
    let descriptor = resolve_connection_descriptor(config, |name| std::env::var_os(name))?;
    connect_pool_with_descriptor(config, &descriptor).await
}

#[allow(
    clippy::disallowed_methods,
    reason = "this helper constructs a PostgreSQL pool, not a SQLite pool"
)]
pub(super) async fn connect_pool_with_descriptor(
    config: &PostgresNamespaceConfig,
    descriptor: &PostgresMtlsConnectionDescriptor,
) -> anyhow::Result<PgPool> {
    let connect_options = descriptor
        .connect_options()
        .application_name("codex-runtime-state-schema")
        .options([(
            "statement_timeout",
            config.pool.statement_timeout.as_millis().to_string(),
        )])
        .log_statements(LevelFilter::Off)
        .log_slow_statements(LevelFilter::Off, Duration::ZERO);
    PgPoolOptions::new()
        .max_connections(config.pool.max_connections.get())
        .acquire_timeout(config.pool.acquire_timeout)
        .after_connect(|connection, _metadata| {
            Box::pin(async move { validate_physical_connection(connection).await })
        })
        .connect_with(connect_options)
        .await
        .map_err(|_| connection_failed(&config.url_env))
}

fn resolve_url(
    config: &PostgresNamespaceConfig,
    get_environment_variable: impl FnOnce(&str) -> Option<OsString>,
) -> anyhow::Result<String> {
    let value = get_environment_variable(&config.url_env).ok_or_else(|| {
        anyhow!(
            "PostgreSQL URL environment variable `{}` is not set; set it to a PostgreSQL connection URL and retry",
            config.url_env
        )
    })?;
    let value = value.into_string().map_err(|_| {
        anyhow!(
            "PostgreSQL URL environment variable `{}` is not valid Unicode; set it to a PostgreSQL connection URL and retry",
            config.url_env
        )
    })?;
    if value.is_empty() {
        anyhow::bail!(
            "PostgreSQL URL environment variable `{}` is empty; set it to a passwordless PostgreSQL mTLS Connection Descriptor and retry",
            config.url_env
        );
    }
    Ok(value)
}

pub(super) fn resolve_connection_descriptor(
    config: &PostgresNamespaceConfig,
    get_environment_variable: impl FnOnce(&str) -> Option<OsString>,
) -> anyhow::Result<PostgresMtlsConnectionDescriptor> {
    let resolved_url = resolve_url(config, get_environment_variable)?;
    PostgresMtlsConnectionDescriptor::parse(&resolved_url, &config.url_env)
}

fn validate_url_environment_variable(name: &str) -> anyhow::Result<()> {
    let mut bytes = name.bytes();
    let first_is_valid = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    if !first_is_valid || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        anyhow::bail!(
            "PostgreSQL URL environment-variable reference must contain only letters, digits, and underscores and begin with a letter or underscore"
        );
    }
    Ok(())
}

fn validate_schema_name(schema: &str) -> anyhow::Result<()> {
    let mut bytes = schema.bytes();
    let first_is_valid = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    if schema.len() > 63
        || !first_is_valid
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        anyhow::bail!(
            "PostgreSQL schema name `{schema}` is invalid; use 1-63 bytes containing only letters, digits, and underscores, beginning with a letter or underscore"
        );
    }
    Ok(())
}

pub(crate) fn connection_failed(name: &str) -> anyhow::Error {
    anyhow!(
        "could not connect to PostgreSQL using environment variable `{name}`; check the mTLS descriptor, TLS files, session evidence, and network reachability"
    )
}
