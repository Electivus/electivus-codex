use anyhow::anyhow;
use sqlx::AssertSqlSafe;
use sqlx::Connection;
use sqlx::PgConnection;
use sqlx::Postgres;
use sqlx::Row;
use sqlx::Transaction;

#[path = "postgres/config.rs"]
mod config;

pub use config::PostgresNamespaceConfig;
pub use config::PostgresPoolConfig;
use config::connect_pool;
use config::connection_failed;

const MIGRATION_TABLE: &str = "_codex_runtime_state_migrations";
const MINIMUM_POSTGRES_MAJOR_VERSION: i32 = 18;
const MINIMUM_COMPATIBLE_SCHEMA_VERSION: i64 = 1;
const MAXIMUM_COMPATIBLE_SCHEMA_VERSION: i64 = 2;
const BASELINE_SCHEMA_VERSION: i64 = 1;
const LOGS_SCHEMA_VERSION: i64 = 2;
const LOGS_MIGRATION_SQL: &str = include_str!("../postgres_migrations/0002_logs.sql");

/// Explicit operation to perform on a PostgreSQL Runtime State Namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostgresNamespaceAction {
    /// Create the schema when absent and apply supported schema migrations.
    Migrate,
    /// Read and validate server and schema versions without executing DDL.
    Validate,
}

/// Compatible schema state returned by a successful namespace operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostgresNamespaceStatus {
    schema: String,
    version: i64,
}

impl PostgresNamespaceStatus {
    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn version(&self) -> i64 {
        self.version
    }

    pub fn compatible_versions(&self) -> std::ops::RangeInclusive<i64> {
        MINIMUM_COMPATIBLE_SCHEMA_VERSION..=MAXIMUM_COMPATIBLE_SCHEMA_VERSION
    }
}

/// Explicitly migrate or validate one PostgreSQL Runtime State Namespace.
///
/// This function is separate from [`crate::StateRuntime`] so normal runtime
/// construction cannot select PostgreSQL and never performs PostgreSQL DDL.
pub async fn manage_postgres_namespace(
    config: PostgresNamespaceConfig,
    action: PostgresNamespaceAction,
) -> anyhow::Result<PostgresNamespaceStatus> {
    let pool = connect_pool(&config).await?;
    manage_postgres_namespace_with_pool(&config, pool, action).await
}

async fn manage_postgres_namespace_with_pool(
    config: &PostgresNamespaceConfig,
    pool: sqlx::PgPool,
    action: PostgresNamespaceAction,
) -> anyhow::Result<PostgresNamespaceStatus> {
    let result = async {
        let mut connection = pool
            .acquire()
            .await
            .map_err(|_| connection_failed(&config.url_env))?;
        manage_postgres_namespace_with_connection(config, &mut connection, action).await
    }
    .await;
    pool.close().await;
    result
}

async fn manage_postgres_namespace_with_connection(
    config: &PostgresNamespaceConfig,
    connection: &mut PgConnection,
    action: PostgresNamespaceAction,
) -> anyhow::Result<PostgresNamespaceStatus> {
    validate_postgres_version(connection, &config.schema).await?;
    match action {
        PostgresNamespaceAction::Migrate => migrate_namespace(connection, &config.schema).await,
        PostgresNamespaceAction::Validate => validate_namespace(connection, &config.schema).await,
    }
}

async fn validate_postgres_version(
    connection: &mut PgConnection,
    schema: &str,
) -> anyhow::Result<()> {
    let server_version_num: i32 =
        sqlx::query_scalar("SELECT current_setting('server_version_num')::int4")
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| map_sql_error(schema, "read server version", error))?;
    let detected_major = server_version_num / 10_000;
    ensure_supported_postgres_version(detected_major)
}

async fn migrate_namespace(
    connection: &mut PgConnection,
    schema: &str,
) -> anyhow::Result<PostgresNamespaceStatus> {
    let mut transaction = connection
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin migration", error))?;
    acquire_namespace_lock(&mut transaction, schema).await?;

    if !schema_exists(transaction.as_mut(), schema).await? {
        let quoted_schema = quote_identifier(schema);
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {quoted_schema}")))
            .execute(transaction.as_mut())
            .await
            .map_err(|error| map_sql_error(schema, "create schema", error))?;
    }

    if !migration_table_exists(transaction.as_mut(), schema).await? {
        create_migration_table(&mut transaction, schema).await?;
    }
    let history = read_migration_history(transaction.as_mut(), schema).await?;
    let mut current_version = match history.current_version(schema)? {
        Some(version) => {
            ensure_compatible_schema_version(schema, version)?;
            version
        }
        None => {
            record_migration_version(&mut transaction, schema, BASELINE_SCHEMA_VERSION).await?;
            BASELINE_SCHEMA_VERSION
        }
    };
    if current_version < LOGS_SCHEMA_VERSION {
        apply_logs_migration(&mut transaction, schema).await?;
        current_version = LOGS_SCHEMA_VERSION;
    }
    transaction
        .commit()
        .await
        .map_err(|error| map_sql_error(schema, "commit migration", error))?;
    Ok(PostgresNamespaceStatus {
        schema: schema.to_string(),
        version: current_version,
    })
}

async fn validate_namespace(
    connection: &mut PgConnection,
    schema: &str,
) -> anyhow::Result<PostgresNamespaceStatus> {
    if !schema_exists(connection, schema).await? {
        anyhow::bail!(
            "PostgreSQL schema `{schema}` does not exist; run `codex state schema migrate` first"
        );
    }
    if !migration_table_exists(connection, schema).await? {
        return Err(migration_history_absent(schema));
    }
    let history = read_migration_history(connection, schema).await?;
    let version = history
        .current_version(schema)?
        .ok_or_else(|| migration_history_absent(schema))?;
    ensure_compatible_schema_version(schema, version)?;
    Ok(PostgresNamespaceStatus {
        schema: schema.to_string(),
        version,
    })
}

async fn acquire_namespace_lock(
    transaction: &mut Transaction<'_, Postgres>,
    schema: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(\
         hashtextextended(current_database() || ':codex-runtime-state:' || $1, 0))",
    )
    .bind(schema)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "acquire namespace migration lock", error))?;
    Ok(())
}

async fn schema_exists(connection: &mut PgConnection, schema: &str) -> anyhow::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname = $1)")
        .bind(schema)
        .fetch_one(connection)
        .await
        .map_err(|error| map_sql_error(schema, "inspect schema", error))
}

async fn migration_table_exists(
    connection: &mut PgConnection,
    schema: &str,
) -> anyhow::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(\
         SELECT 1 FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('r', 'p'))",
    )
    .bind(schema)
    .bind(MIGRATION_TABLE)
    .fetch_one(connection)
    .await
    .map_err(|error| map_sql_error(schema, "inspect migration history", error))
}

async fn create_migration_table(
    transaction: &mut Transaction<'_, Postgres>,
    schema: &str,
) -> anyhow::Result<()> {
    let table = qualified_migration_table(schema);
    sqlx::query(AssertSqlSafe(format!(
        "CREATE TABLE {table} (\
         version BIGINT PRIMARY KEY CHECK (version > 0), \
         applied_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP)"
    )))
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "create migration history", error))?;
    Ok(())
}

async fn apply_logs_migration(
    transaction: &mut Transaction<'_, Postgres>,
    schema: &str,
) -> anyhow::Result<()> {
    sqlx::query("SELECT set_config('search_path', $1, true)")
        .bind(quote_identifier(schema))
        .execute(transaction.as_mut())
        .await
        .map_err(|error| map_sql_error(schema, "select migration schema", error))?;
    sqlx::raw_sql(LOGS_MIGRATION_SQL)
        .execute(transaction.as_mut())
        .await
        .map_err(|error| map_sql_error(schema, "create logs storage", error))?;
    record_migration_version(transaction, schema, LOGS_SCHEMA_VERSION).await
}

async fn record_migration_version(
    transaction: &mut Transaction<'_, Postgres>,
    schema: &str,
    version: i64,
) -> anyhow::Result<()> {
    let table = qualified_migration_table(schema);
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {table} (version) VALUES ($1)"
    )))
    .bind(version)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "record migration version", error))?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MigrationHistory {
    minimum: Option<i64>,
    maximum: Option<i64>,
    count: i64,
}

impl MigrationHistory {
    fn current_version(self, schema: &str) -> anyhow::Result<Option<i64>> {
        match (self.minimum, self.maximum, self.count) {
            (None, None, 0) => Ok(None),
            (Some(1), Some(maximum), count) if maximum == count => Ok(Some(maximum)),
            (None, None, _) | (None, Some(_), _) | (Some(_), None, _) | (Some(_), Some(_), _) => {
                Err(anyhow!(
                    "PostgreSQL schema `{schema}` has an invalid Codex migration history; restore it or provision a new Runtime State Namespace"
                ))
            }
        }
    }
}

async fn read_migration_history(
    connection: &mut PgConnection,
    schema: &str,
) -> anyhow::Result<MigrationHistory> {
    let table = qualified_migration_table(schema);
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT MIN(version), MAX(version), COUNT(*) FROM {table}"
    )))
    .fetch_one(connection)
    .await
    .map_err(|error| map_sql_error(schema, "read migration history", error))?;
    Ok(MigrationHistory {
        minimum: row
            .try_get(0)
            .map_err(|error| map_sql_error(schema, "decode migration history", error))?,
        maximum: row
            .try_get(1)
            .map_err(|error| map_sql_error(schema, "decode migration history", error))?,
        count: row
            .try_get(2)
            .map_err(|error| map_sql_error(schema, "decode migration history", error))?,
    })
}

fn ensure_compatible_schema_version(schema: &str, version: i64) -> anyhow::Result<()> {
    if version < MINIMUM_COMPATIBLE_SCHEMA_VERSION {
        anyhow::bail!(
            "PostgreSQL schema `{schema}` is at version {version}, older than the minimum supported version {MINIMUM_COMPATIBLE_SCHEMA_VERSION}; run a compatible Codex schema migration command"
        );
    }
    if version > MAXIMUM_COMPATIBLE_SCHEMA_VERSION {
        anyhow::bail!(
            "PostgreSQL schema `{schema}` is at version {version}, newer than the maximum supported version {MAXIMUM_COMPATIBLE_SCHEMA_VERSION}; upgrade Codex before using this namespace"
        );
    }
    Ok(())
}

fn ensure_supported_postgres_version(detected_major: i32) -> anyhow::Result<()> {
    if detected_major < MINIMUM_POSTGRES_MAJOR_VERSION {
        anyhow::bail!(
            "PostgreSQL {detected_major} is unsupported; PostgreSQL {MINIMUM_POSTGRES_MAJOR_VERSION} or later is required"
        );
    }
    Ok(())
}

fn qualified_migration_table(schema: &str) -> String {
    let schema = quote_identifier(schema);
    let table = quote_identifier(MIGRATION_TABLE);
    format!("{schema}.{table}")
}

pub(crate) fn qualified_table(schema: &str, table: &str) -> String {
    format!("{}.{}", quote_identifier(schema), quote_identifier(table))
}

pub(crate) fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

pub(crate) fn map_sql_error(
    schema: &str,
    operation: &'static str,
    error: sqlx::Error,
) -> anyhow::Error {
    match error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
    {
        Some(code) if code == "42501" => anyhow!(
            "PostgreSQL denied the `{operation}` operation for schema `{schema}`; check the configured role's privileges"
        ),
        Some(code) if code == "57014" => anyhow!(
            "PostgreSQL timed out during the `{operation}` operation for schema `{schema}`; retry or increase the configured statement timeout"
        ),
        Some(_) | None => anyhow!(
            "PostgreSQL could not complete the `{operation}` operation for schema `{schema}`; verify the namespace and server health, then retry"
        ),
    }
}

fn migration_history_absent(schema: &str) -> anyhow::Error {
    anyhow!(
        "PostgreSQL schema `{schema}` has no Codex migration history; run `codex state schema migrate` on an empty schema first"
    )
}

#[cfg(test)]
#[path = "postgres_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "postgres/test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "postgres_contract_tests.rs"]
mod contract_tests;
