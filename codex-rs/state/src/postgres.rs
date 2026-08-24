use anyhow::anyhow;
use sqlx::AssertSqlSafe;
use sqlx::Connection;
use sqlx::PgConnection;
use sqlx::Postgres;
use sqlx::Row;
use sqlx::Transaction;

#[path = "postgres/config.rs"]
pub(crate) mod config;
#[path = "postgres/connection_descriptor.rs"]
mod connection_descriptor;
#[path = "postgres/connection_validation.rs"]
mod connection_validation;
#[path = "postgres/initialize.rs"]
mod initialize;
#[path = "postgres/readiness.rs"]
mod readiness;

pub use config::PostgresNamespaceConfig;
pub use config::PostgresPoolConfig;
use config::connect_pool;
use config::connection_failed;
pub use initialize::RuntimeStateInitializationReport;
pub use initialize::initialize_postgres_runtime_state;

const MIGRATION_TABLE: &str = "_codex_runtime_state_migrations";
const MINIMUM_POSTGRES_MAJOR_VERSION: i32 = 18;
const MINIMUM_COMPATIBLE_SCHEMA_VERSION: i64 = 1;
pub(crate) const MAXIMUM_COMPATIBLE_SCHEMA_VERSION: i64 = 24;
const BASELINE_SCHEMA_VERSION: i64 = 1;
const MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        2,
        include_str!("../postgres_migrations/0002_logs.sql"),
        "create logs storage",
    ),
    (
        3,
        include_str!("../postgres_migrations/0003_remote_control_enrollments.sql"),
        "create remote control storage",
    ),
    (
        4,
        include_str!("../postgres_migrations/0004_threads.sql"),
        "create thread storage",
    ),
    (
        5,
        include_str!("../postgres_migrations/0005_thread_item_projections.sql"),
        "create thread item projections",
    ),
    (
        6,
        include_str!("../postgres_migrations/0006_thread_turn_projections.sql"),
        "create thread turn projections",
    ),
    (
        7,
        include_str!("../postgres_migrations/0007_thread_history_projection_state.sql"),
        "track thread history projection state",
    ),
    (
        8,
        include_str!("../postgres_migrations/0008_thread_search_projection.sql"),
        "create thread search projection",
    ),
    (
        9,
        include_str!("../postgres_migrations/0009_backfill_coordination.sql"),
        "create backfill coordination",
    ),
    (
        10,
        include_str!("../postgres_migrations/0010_thread_goals.sql"),
        "create thread goal storage",
    ),
    (
        11,
        include_str!("../postgres_migrations/0011_thread_goal_accounting_events.sql"),
        "create thread goal accounting storage",
    ),
    (
        12,
        include_str!("../postgres_migrations/0012_memory_stage1.sql"),
        "create stage-one memory storage",
    ),
    (
        13,
        include_str!("../postgres_migrations/0013_memory_thread_mode_overrides.sql"),
        "track runtime memory pollution",
    ),
    (
        14,
        include_str!("../postgres_migrations/0014_memory_generations.sql"),
        "create atomic memory generation storage",
    ),
    (
        15,
        include_str!("../postgres_migrations/0015_external_agent_config_imports.sql"),
        "create external-agent import storage",
    ),
    (
        16,
        include_str!("../postgres_migrations/0016_external_agent_memory_imports.sql"),
        "link external-agent imports to Memory Generations",
    ),
    (
        17,
        include_str!("../postgres_migrations/0017_runtime_state_migration.sql"),
        "prepare explicit Runtime State Migration",
    ),
    (
        18,
        include_str!("../postgres_migrations/0018_external_agent_config_imports_provider_id.sql"),
        "track external-agent provider identifiers",
    ),
    (
        19,
        include_str!("../postgres_migrations/0019_thread_item_update_ordinals.sql"),
        "track thread item update ordinals",
    ),
    (
        20,
        include_str!("../postgres_migrations/0020_threads_is_pinned.sql"),
        "track pinned threads",
    ),
    (
        21,
        include_str!("../postgres_migrations/0021_threads_repository_identity.sql"),
        "materialize thread repository identity",
    ),
    (
        22,
        include_str!("../postgres_migrations/0022_threads_section.sql"),
        "create thread sections",
    ),
    (
        23,
        include_str!("../postgres_migrations/0023_threads_section_order.sql"),
        "track thread section ordering",
    ),
    (
        24,
        include_str!("../postgres_migrations/0024_thread_section_appearance.sql"),
        "add thread section appearance",
    ),
];

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

/// Owns the shared connection pool for one pre-migrated PostgreSQL Runtime State Namespace.
///
/// Construct this once for each runtime replica and derive storage facades from it. Connecting
/// validates the namespace without executing DDL; migrations remain an explicit
/// [`manage_postgres_namespace`] operation.
#[derive(Clone)]
pub struct PostgresRuntimeStatePool {
    pool: sqlx::PgPool,
    schema: String,
}

impl PostgresRuntimeStatePool {
    /// Connects one pool and validates that its namespace is ready for runtime traffic.
    ///
    /// Validation runs in a read-only transaction and requires the final migration fence. It
    /// never creates or migrates schema objects.
    pub async fn connect(config: PostgresNamespaceConfig) -> anyhow::Result<Self> {
        let pool = connect_pool(&config).await?;
        let validation = readiness::validate_runtime_readiness(&config, &pool).await;
        if let Err(error) = validation {
            pool.close().await;
            return Err(error);
        }
        Ok(Self {
            pool,
            schema: config.schema,
        })
    }

    /// Connects to a compatible namespace before the final readiness fence for state-crate tests.
    #[cfg(test)]
    pub(crate) async fn connect_for_migration(
        config: PostgresNamespaceConfig,
    ) -> anyhow::Result<Self> {
        let pool = connect_pool(&config).await?;
        let validation = async {
            let mut connection = pool
                .acquire()
                .await
                .map_err(|_| connection_failed(&config))?;
            manage_postgres_namespace_with_connection(
                &config,
                &mut connection,
                PostgresNamespaceAction::Validate,
            )
            .await
        }
        .await;
        if let Err(error) = validation {
            pool.close().await;
            return Err(error);
        }
        Ok(Self {
            pool,
            schema: config.schema,
        })
    }

    /// Derives an enrollment facade that shares this owner's pool.
    pub fn remote_control_enrollment_store(&self) -> crate::RemoteControlEnrollmentStore {
        crate::RemoteControlEnrollmentStore::from_postgres(self.pool.clone(), self.schema.clone())
    }

    /// Derives a rollout metadata backfill coordinator that shares this owner's pool.
    pub fn backfill_coordinator(&self) -> crate::BackfillCoordinator {
        crate::BackfillCoordinator::from_postgres(self.pool.clone(), self.schema.clone())
    }

    /// Derives a goal facade that shares this owner's pool.
    pub fn goal_store(&self) -> crate::GoalStore {
        crate::GoalStore::from_postgres(self.pool.clone(), self.schema.clone())
    }

    /// Derives a memory facade that shares this owner's pool.
    pub fn memory_store(&self) -> crate::MemoryStore {
        crate::MemoryStore::from_postgres(self.pool.clone(), self.schema.clone())
    }

    /// Derives an external-agent import facade that shares this owner's pool.
    pub fn external_agent_config_import_store(&self) -> crate::ExternalAgentConfigImportStore {
        crate::ExternalAgentConfigImportStore::from_postgres(self.pool.clone(), self.schema.clone())
    }

    /// Returns the shared pool and namespace needed by the thread-store facade.
    ///
    /// This is an internal workspace integration seam. Application code should construct runtime
    /// state responsibilities from this owner instead of issuing SQL directly.
    #[doc(hidden)]
    pub fn thread_store_connection(&self) -> (sqlx::PgPool, String) {
        (self.pool.clone(), self.schema.clone())
    }

    /// Closes the shared pool and waits for checked-out connections to return.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// Explicitly migrate or validate one PostgreSQL Runtime State Namespace.
///
/// This function is separate from [`crate::StateRuntime`] so normal runtime
/// construction can validate PostgreSQL without ever performing PostgreSQL DDL.
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
            .map_err(|_| connection_failed(config))?;
        manage_postgres_namespace_with_connection(config, &mut connection, action).await
    }
    .await;
    pool.close().await;
    result
}

pub(crate) async fn manage_postgres_namespace_with_connection(
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
    for &(version, sql, operation) in MIGRATIONS {
        if current_version < version {
            apply_namespace_migration(&mut transaction, schema, sql, version, operation).await?;
            current_version = version;
        }
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

pub(crate) async fn acquire_namespace_lock(
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

async fn apply_namespace_migration(
    transaction: &mut Transaction<'_, Postgres>,
    schema: &str,
    sql: &'static str,
    version: i64,
    operation: &'static str,
) -> anyhow::Result<()> {
    sqlx::query("SELECT set_config('search_path', $1, true)")
        .bind(quote_identifier(schema))
        .execute(transaction.as_mut())
        .await
        .map_err(|error| map_sql_error(schema, "select migration schema", error))?;
    sqlx::raw_sql(sql)
        .execute(transaction.as_mut())
        .await
        .map_err(|error| map_sql_error(schema, operation, error))?;
    record_migration_version(transaction, schema, version).await
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
pub(crate) mod test_support;

#[cfg(test)]
#[path = "postgres_contract_tests.rs"]
mod contract_tests;

#[cfg(test)]
#[path = "postgres_initialize_contract_tests.rs"]
mod initialize_contract_tests;

#[cfg(test)]
#[path = "postgres_external_agent_memory_import_contract_tests.rs"]
mod external_agent_memory_import_contract_tests;

#[cfg(test)]
#[path = "postgres_memory_concurrency_contract_tests.rs"]
mod memory_concurrency_contract_tests;

#[cfg(test)]
#[path = "postgres_memory_generation_contract_tests.rs"]
mod memory_generation_contract_tests;

#[cfg(test)]
#[path = "postgres_memory_reset_contract_tests.rs"]
mod memory_reset_contract_tests;

#[cfg(test)]
#[path = "postgres_migration_cleanup_contract_tests.rs"]
mod migration_cleanup_contract_tests;
