use super::MigrationHistory;
use super::PostgresNamespaceAction;
use super::PostgresNamespaceConfig;
use super::PostgresNamespaceStatus;
use super::PostgresPoolConfig;
use super::acquire_namespace_lock;
use super::config::connect_pool_with_descriptor;
use super::connection_descriptor::PostgresMtlsConnectionDescriptor;
use super::connection_failed;
use super::manage_postgres_namespace_with_connection;
use super::manage_postgres_namespace_with_pool;
use super::map_sql_error;
use super::quote_identifier;
use super::read_migration_history;
use super::schema_exists;
use anyhow::Context;
use sqlx::AssertSqlSafe;
use sqlx::Connection;
use sqlx::PgPool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use tokio::time::Instant;

pub(super) const TEST_DATABASE_URL_ENV: &str = "CODEX_TEST_POSTGRES_URL";

static NEXT_NAMESPACE_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) fn test_database_url() -> anyhow::Result<String> {
    std::env::var_os(TEST_DATABASE_URL_ENV)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "PostgreSQL contract test requires `{TEST_DATABASE_URL_ENV}` to contain a dedicated PostgreSQL 18 test URL"
            )
        })?
        .into_string()
        .map_err(|_| {
            anyhow::anyhow!(
                "PostgreSQL test URL environment variable `{TEST_DATABASE_URL_ENV}` is not valid Unicode"
            )
        })
}

#[derive(Debug)]
pub(crate) struct PostgresContractFixture {
    config: PostgresNamespaceConfig,
    descriptor: PostgresMtlsConnectionDescriptor,
    cleanup_confirmed: bool,
}

impl PostgresContractFixture {
    pub(crate) fn new(database_url: String, group: &str) -> anyhow::Result<Self> {
        let schema = unique_schema_name(group)?;
        let config = PostgresNamespaceConfig::new(
            TEST_DATABASE_URL_ENV.to_string(),
            schema,
            PostgresPoolConfig::default(),
        )?;
        let descriptor =
            PostgresMtlsConnectionDescriptor::parse(&database_url, TEST_DATABASE_URL_ENV)?;
        Ok(Self {
            config,
            descriptor,
            cleanup_confirmed: false,
        })
    }

    pub(crate) fn schema(&self) -> &str {
        &self.config.schema
    }

    pub(crate) fn config_for_tests(&self) -> PostgresNamespaceConfig {
        self.config.clone()
    }

    pub(crate) async fn manage(
        &self,
        action: PostgresNamespaceAction,
    ) -> anyhow::Result<PostgresNamespaceStatus> {
        let pool = self.connect_pool().await?;
        manage_postgres_namespace_with_pool(&self.config, pool, action).await
    }

    pub(super) async fn validate_read_only(&self) -> anyhow::Result<PostgresNamespaceStatus> {
        let pool = self.connect_pool().await?;
        let result: anyhow::Result<PostgresNamespaceStatus> = async {
            let mut connection = pool
                .acquire()
                .await
                .map_err(|_| connection_failed(&self.config.url_env))?;
            let mut transaction = connection.begin().await.map_err(|error| {
                map_sql_error(self.schema(), "begin read-only validation", error)
            })?;
            sqlx::query("SET TRANSACTION READ ONLY")
                .execute(transaction.as_mut())
                .await
                .map_err(|error| {
                    map_sql_error(self.schema(), "make validation read-only", error)
                })?;
            let status = manage_postgres_namespace_with_connection(
                &self.config,
                transaction.as_mut(),
                PostgresNamespaceAction::Validate,
            )
            .await?;
            transaction.rollback().await.map_err(|error| {
                map_sql_error(self.schema(), "finish read-only validation", error)
            })?;
            Ok(status)
        }
        .await;
        pool.close().await;
        result
    }

    pub(crate) async fn schema_exists(&self) -> anyhow::Result<bool> {
        let pool = self.connect_pool().await?;
        let result = async {
            let mut connection = pool
                .acquire()
                .await
                .map_err(|_| connection_failed(&self.config.url_env))?;
            schema_exists(&mut connection, self.schema()).await
        }
        .await;
        pool.close().await;
        result
    }

    pub(super) async fn migration_history(&self) -> anyhow::Result<MigrationHistory> {
        let pool = self.connect_pool().await?;
        let result = async {
            let mut connection = pool
                .acquire()
                .await
                .map_err(|_| connection_failed(&self.config.url_env))?;
            read_migration_history(&mut connection, self.schema()).await
        }
        .await;
        pool.close().await;
        result
    }

    pub(super) async fn hold_migration_lock(&self) -> anyhow::Result<HeldMigrationLock> {
        let pool = self.connect_pool().await?;
        let mut transaction = pool
            .begin()
            .await
            .map_err(|error| map_sql_error(self.schema(), "begin test lock", error))?;
        acquire_namespace_lock(&mut transaction, self.schema()).await?;
        Ok(HeldMigrationLock {
            pool,
            transaction,
            schema: self.schema().to_string(),
        })
    }

    pub(crate) async fn cleanup(&mut self) -> anyhow::Result<()> {
        let pool = self.connect_pool().await?;
        let result: anyhow::Result<()> = async {
            let mut connection = pool
                .acquire()
                .await
                .map_err(|_| connection_failed(&self.config.url_env))?;
            let schema = quote_identifier(self.schema());
            sqlx::query(AssertSqlSafe(format!(
                "DROP SCHEMA IF EXISTS {schema} CASCADE"
            )))
            .execute(&mut *connection)
            .await
            .map_err(|error| map_sql_error(self.schema(), "clean up test schema", error))?;
            Ok(())
        }
        .await;
        pool.close().await;
        result?;
        self.cleanup_confirmed = true;
        Ok(())
    }

    pub(crate) async fn connect_pool(&self) -> anyhow::Result<PgPool> {
        connect_pool_with_descriptor(&self.config, &self.descriptor).await
    }

    pub(crate) async fn mark_runtime_ready_for_tests(&self) -> anyhow::Result<()> {
        let pool = self.connect_pool().await?;
        let migration = super::qualified_table(self.schema(), "runtime_state_migration");
        let evidence = serde_json::json!({
            "sourceIdentity": "contract-source",
            "sourceFingerprint": "contract-fingerprint",
            "phase": "ready",
            "ready": true,
            "fencingToken": 4,
            "namespaceDigest": "contract-final-digest",
            "globalReferentialIntegrityValidated": true,
            "canonicalThreadHistoryOrderingValidated": true,
        });
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {migration} (source_identity, source_fingerprint, phase, ready, \
             phase_evidence, fencing_token) VALUES ($1, $2, 'ready', TRUE, $3, 4)"
        )))
        .bind("contract-source")
        .bind("contract-fingerprint")
        .bind(evidence)
        .execute(&pool)
        .await
        .map_err(|error| map_sql_error(self.schema(), "mark test namespace ready", error))?;
        pool.close().await;
        Ok(())
    }
}

impl Drop for PostgresContractFixture {
    fn drop(&mut self) {
        if !self.cleanup_confirmed {
            eprintln!(
                "PostgreSQL contract schema `{}` was preserved for failure diagnosis",
                self.schema()
            );
        }
    }
}

pub(super) struct HeldMigrationLock {
    pool: PgPool,
    transaction: sqlx::Transaction<'static, sqlx::Postgres>,
    schema: String,
}

impl HeldMigrationLock {
    pub(super) async fn wait_for_waiter(&mut self) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let schema = self.schema.clone();
        loop {
            let waiting: bool = sqlx::query_scalar(
                "WITH lock_key AS (\
                 SELECT hashtextextended(\
                 current_database() || ':codex-runtime-state:' || $1, 0\
                 ) AS value\
                 ) \
                 SELECT EXISTS(\
                 SELECT 1 FROM pg_locks AS locks CROSS JOIN lock_key \
                 WHERE locks.locktype = 'advisory' \
                 AND locks.database = (\
                 SELECT oid FROM pg_database WHERE datname = current_database()\
                 ) \
                 AND locks.classid = ((lock_key.value >> 32) & 4294967295)::oid \
                 AND locks.objid = (lock_key.value & 4294967295)::oid \
                 AND locks.objsubid = 1 \
                 AND NOT locks.granted\
                 )",
            )
            .bind(&schema)
            .fetch_one(self.transaction.as_mut())
            .await
            .map_err(|error| map_sql_error(&schema, "observe test lock waiter", error))?;
            if waiting {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "timed out waiting for a migration to contend on PostgreSQL contract schema `{schema}`"
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub(super) async fn release(self) -> anyhow::Result<()> {
        let Self {
            pool,
            transaction,
            schema,
        } = self;
        transaction
            .commit()
            .await
            .map_err(|error| map_sql_error(&schema, "release test lock", error))?;
        pool.close().await;
        Ok(())
    }
}

fn unique_schema_name(group: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        !group.is_empty()
            && group
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
        "PostgreSQL contract group names must use lowercase ASCII letters, digits, or underscores"
    );
    let elapsed = SystemTime::UNIX_EPOCH
        .elapsed()
        .context("system clock is before the Unix epoch")?;
    let sequence = NEXT_NAMESPACE_ID.fetch_add(1, Ordering::Relaxed);
    Ok(format!(
        "codex_contract_{group}_{:x}_{:x}_{sequence:x}",
        std::process::id(),
        elapsed.as_nanos()
    ))
}
