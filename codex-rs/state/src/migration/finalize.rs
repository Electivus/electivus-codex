use super::RuntimeStateMigrationInventory;
use super::RuntimeStateMigrationPhase;
use super::destination_validation;
use super::import_threads::fingerprint;
use super::import_threads::revalidate_source;
use super::import_threads::source_identity;
use super::progress::RuntimeStateMigrationEvidence;
use super::progress::existing_progress;
use super::progress::namespace_digest;
use super::progress::phase_evidence;
use crate::PostgresNamespaceAction;
use crate::PostgresNamespaceConfig;
use crate::SqliteConfig;
use crate::postgres::MAXIMUM_COMPATIBLE_SCHEMA_VERSION;
use crate::postgres::acquire_namespace_lock;
use crate::postgres::config::connect_pool;
use crate::postgres::manage_postgres_namespace_with_connection;
use crate::postgres::map_sql_error;
use crate::postgres::qualified_table;
use anyhow::Context;
use serde_json::Value;
use sqlx::AssertSqlSafe;

/// Bounded integrity evidence for a completed, ready Runtime State Migration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeStateMigrationReport {
    destination_schema: String,
    fencing_token: i64,
    evidence: Value,
}

impl RuntimeStateMigrationReport {
    pub fn destination_schema(&self) -> &str {
        &self.destination_schema
    }

    pub fn fencing_token(&self) -> i64 {
        self.fencing_token
    }

    pub fn evidence(&self) -> &Value {
        &self.evidence
    }
}

/// Validate the complete imported namespace and atomically mark it ready for manual cutover.
pub(super) async fn finalize_runtime_state_migration(
    source: &SqliteConfig,
    destination: &PostgresNamespaceConfig,
    expected_inventory: &RuntimeStateMigrationInventory,
) -> anyhow::Result<RuntimeStateMigrationReport> {
    anyhow::ensure!(
        expected_inventory.destination_schema == destination.schema()
            && expected_inventory.destination_schema_version == MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
        "Runtime State Migration inventory does not match the current PostgreSQL destination"
    );
    revalidate_source(source, expected_inventory).await?;
    let source_identity = source_identity(source);
    let source_fingerprint = fingerprint(expected_inventory);
    let pool = connect_pool(destination).await?;
    let result = finalize(
        source,
        destination,
        expected_inventory,
        &source_identity,
        &source_fingerprint,
        &pool,
    )
    .await;
    pool.close().await;
    result
}

async fn finalize(
    source: &SqliteConfig,
    destination: &PostgresNamespaceConfig,
    expected_inventory: &RuntimeStateMigrationInventory,
    source_identity: &str,
    source_fingerprint: &str,
    pool: &sqlx::PgPool,
) -> anyhow::Result<RuntimeStateMigrationReport> {
    let schema = destination.schema();
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin final migration validation", error))?;
    acquire_namespace_lock(&mut transaction, schema).await?;
    let status = manage_postgres_namespace_with_connection(
        destination,
        transaction.as_mut(),
        PostgresNamespaceAction::Validate,
    )
    .await?;
    anyhow::ensure!(
        status.version() == MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
        "PostgreSQL Runtime State Namespace changed after migration preflight"
    );
    let progress = existing_progress(
        transaction.as_mut(),
        schema,
        source_identity,
        source_fingerprint,
    )
    .await?
    .context("Runtime State Migration must import memory before final readiness validation")?;
    anyhow::ensure!(
        progress.phase == RuntimeStateMigrationPhase::MemoryImported && progress.fencing_token == 3,
        "Runtime State Migration must complete thread, operational, and memory imports before readiness"
    );
    destination_validation::validate_layout(transaction.as_mut(), schema).await?;
    validate_global_integrity(transaction.as_mut(), schema).await?;
    revalidate_source(source, expected_inventory).await?;

    let migration = qualified_table(schema, "runtime_state_migration");
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {migration} SET phase = 'ready', ready = TRUE, phase_evidence = '{{}}'::jsonb, \
         fencing_token = 4, updated_at = CURRENT_TIMESTAMP WHERE singleton"
    )))
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "record final migration readiness", error))?;
    let digest = namespace_digest(transaction.as_mut(), schema).await?;
    let mut evidence = phase_evidence(
        transaction.as_mut(),
        schema,
        RuntimeStateMigrationEvidence {
            source_identity,
            source_fingerprint,
            phase: RuntimeStateMigrationPhase::Ready,
            ready: true,
            fencing_token: 4,
            namespace_digest: &digest,
        },
    )
    .await?;
    evidence["globalReferentialIntegrityValidated"] = Value::Bool(true);
    evidence["canonicalThreadHistoryOrderingValidated"] = Value::Bool(true);
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {migration} SET phase_evidence = $1 WHERE singleton"
    )))
    .bind(&evidence)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "seal final migration evidence", error))?;
    transaction
        .commit()
        .await
        .map_err(|error| map_sql_error(schema, "commit final migration readiness", error))?;
    Ok(RuntimeStateMigrationReport {
        destination_schema: schema.to_string(),
        fencing_token: 4,
        evidence,
    })
}

pub(crate) async fn validate_global_integrity(
    connection: &mut sqlx::PgConnection,
    schema: &str,
) -> anyhow::Result<()> {
    let invalid_foreign_keys: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_constraint constraint_record \
         JOIN pg_namespace namespace ON namespace.oid = constraint_record.connamespace \
         WHERE namespace.nspname = $1 AND constraint_record.contype = 'f' \
         AND NOT constraint_record.convalidated",
    )
    .bind(schema)
    .fetch_one(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "validate migration foreign keys", error))?;
    anyhow::ensure!(
        invalid_foreign_keys == 0,
        "PostgreSQL Runtime State Namespace has unvalidated referential constraints"
    );

    let threads = qualified_table(schema, "threads");
    let history = qualified_table(schema, "thread_history");
    let invalid_history_streams: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {threads} thread_record WHERE thread_record.stream_version <> \
         (SELECT COUNT(*) FROM {history} history_record \
          WHERE history_record.thread_id = thread_record.thread_id) \
         OR (thread_record.stream_version > 0 AND NOT EXISTS ( \
             SELECT 1 FROM {history} first_record \
             WHERE first_record.thread_id = thread_record.thread_id AND first_record.ordinal = 0)) \
         OR (thread_record.stream_version > 0 AND NOT EXISTS ( \
             SELECT 1 FROM {history} last_record \
             WHERE last_record.thread_id = thread_record.thread_id \
             AND last_record.ordinal = thread_record.stream_version - 1))"
    )))
    .fetch_one(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "validate Canonical Thread History ordering", error))?;
    anyhow::ensure!(
        invalid_history_streams == 0,
        "Canonical Thread History identifiers or ordering are invalid"
    );

    let generation_state = qualified_table(schema, "memory_generation_state");
    let generation_ready: bool = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) = 1 AND BOOL_AND(singleton AND active_generation_id IS NOT NULL) \
         FROM {generation_state}"
    )))
    .fetch_one(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "validate active Memory Generation", error))?;
    anyhow::ensure!(
        generation_ready,
        "Runtime State Namespace does not have one active Memory Generation"
    );
    Ok(())
}
