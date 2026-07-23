use super::MAXIMUM_COMPATIBLE_SCHEMA_VERSION;
use super::PostgresNamespaceAction;
use super::PostgresNamespaceConfig;
use super::acquire_namespace_lock;
use super::config::connect_pool;
use super::manage_postgres_namespace;
use super::manage_postgres_namespace_with_connection;
use super::map_sql_error;
use super::qualified_table;
use crate::MemoryArtifactSet;
use crate::migration::RuntimeStateMigrationEvidence;
use crate::migration::RuntimeStateMigrationPhase;
use crate::migration::destination_validation;
use crate::migration::namespace_digest;
use crate::migration::phase_evidence;
use crate::migration::validate_global_integrity;
use crate::runtime::import_migrated_memory_generation;
use serde_json::Value;
use sqlx::AssertSqlSafe;

pub(super) const EMPTY_SOURCE_IDENTITY: &str = "codex-empty-runtime-state";
pub(super) const EMPTY_SOURCE_FINGERPRINT: &str = "empty-runtime-state-v1";
pub(super) const EMPTY_INITIALIZATION_FENCING_TOKEN: i64 = 1;

/// Readiness evidence produced by an explicit empty Runtime State initialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeStateInitializationReport {
    schema: String,
    fencing_token: i64,
    evidence: Value,
}

impl RuntimeStateInitializationReport {
    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn fencing_token(&self) -> i64 {
        self.fencing_token
    }

    pub fn evidence(&self) -> &Value {
        &self.evidence
    }
}

/// Create, validate, and mark an empty PostgreSQL Runtime State Namespace ready.
///
/// This operation never reads SQLite. It accepts only a newly migrated, empty namespace and
/// publishes the empty Memory Generation required by the Runtime State Contract.
pub async fn initialize_postgres_runtime_state(
    config: PostgresNamespaceConfig,
) -> anyhow::Result<RuntimeStateInitializationReport> {
    let status =
        manage_postgres_namespace(config.clone(), PostgresNamespaceAction::Migrate).await?;
    anyhow::ensure!(
        status.version() == MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
        "PostgreSQL schema `{}` is at version {}, but empty initialization requires the current version {}",
        status.schema(),
        status.version(),
        MAXIMUM_COMPATIBLE_SCHEMA_VERSION
    );

    let pool = connect_pool(&config).await?;
    let result = initialize_empty_namespace(&config, &pool).await;
    pool.close().await;
    result
}

async fn initialize_empty_namespace(
    config: &PostgresNamespaceConfig,
    pool: &sqlx::PgPool,
) -> anyhow::Result<RuntimeStateInitializationReport> {
    let schema = config.schema();
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin empty initialization", error))?;
    acquire_namespace_lock(&mut transaction, schema).await?;
    let status = manage_postgres_namespace_with_connection(
        config,
        transaction.as_mut(),
        PostgresNamespaceAction::Validate,
    )
    .await?;
    anyhow::ensure!(
        status.version() == MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
        "PostgreSQL Runtime State Namespace changed during empty initialization"
    );
    destination_validation::ensure_empty(transaction.as_mut(), schema).await?;

    let artifacts = MemoryArtifactSet::new(Vec::new())?;
    import_migrated_memory_generation(
        &mut transaction,
        pool.clone(),
        schema.to_string(),
        /*completed_watermark*/ 0,
        &artifacts,
    )
    .await?;
    validate_global_integrity(transaction.as_mut(), schema).await?;

    let migration = qualified_table(schema, "runtime_state_migration");
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {migration} (source_identity, source_fingerprint, phase, ready, \
         phase_evidence, fencing_token) VALUES ($1, $2, 'ready', TRUE, '{{}}'::jsonb, $3)"
    )))
    .bind(EMPTY_SOURCE_IDENTITY)
    .bind(EMPTY_SOURCE_FINGERPRINT)
    .bind(EMPTY_INITIALIZATION_FENCING_TOKEN)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "record empty initialization readiness", error))?;

    let digest = namespace_digest(transaction.as_mut(), schema).await?;
    let mut evidence = phase_evidence(
        transaction.as_mut(),
        schema,
        RuntimeStateMigrationEvidence {
            source_identity: EMPTY_SOURCE_IDENTITY,
            source_fingerprint: EMPTY_SOURCE_FINGERPRINT,
            phase: RuntimeStateMigrationPhase::Ready,
            ready: true,
            fencing_token: EMPTY_INITIALIZATION_FENCING_TOKEN,
            namespace_digest: &digest,
        },
    )
    .await?;
    evidence["initializationMode"] = Value::String("empty".to_string());
    evidence["globalReferentialIntegrityValidated"] = Value::Bool(true);
    evidence["canonicalThreadHistoryOrderingValidated"] = Value::Bool(true);
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {migration} SET phase_evidence = $1 WHERE singleton"
    )))
    .bind(&evidence)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "seal empty initialization readiness", error))?;

    transaction
        .commit()
        .await
        .map_err(|error| map_sql_error(schema, "commit empty initialization", error))?;
    Ok(RuntimeStateInitializationReport {
        schema: schema.to_string(),
        fencing_token: EMPTY_INITIALIZATION_FENCING_TOKEN,
        evidence,
    })
}
