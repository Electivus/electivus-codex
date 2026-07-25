use anyhow::anyhow;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Connection;
use sqlx::PgConnection;
use sqlx::Row;

use super::MAXIMUM_COMPATIBLE_SCHEMA_VERSION;
use super::PostgresNamespaceAction;
use super::config::PostgresNamespaceConfig;
use super::config::connection_failed;
use super::initialize;
use super::manage_postgres_namespace_with_connection;
use super::map_sql_error;
use super::qualified_table;

pub(super) async fn validate_runtime_readiness(
    config: &PostgresNamespaceConfig,
    pool: &sqlx::PgPool,
) -> anyhow::Result<()> {
    let schema = config.schema();
    let mut connection = pool
        .acquire()
        .await
        .map_err(|_| connection_failed(config.url_env()))?;
    let mut transaction = connection
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin runtime readiness validation", error))?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(transaction.as_mut())
        .await
        .map_err(|error| {
            map_sql_error(schema, "make runtime readiness validation read-only", error)
        })?;
    let validation = async {
        let status = manage_postgres_namespace_with_connection(
            config,
            transaction.as_mut(),
            PostgresNamespaceAction::Validate,
        )
        .await?;
        anyhow::ensure!(
            status.version() == MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
            "PostgreSQL schema `{}` is at version {}, but runtime use requires the current version {}; run `codex state schema migrate` and retry",
            status.schema(),
            status.version(),
            MAXIMUM_COMPATIBLE_SCHEMA_VERSION
        );
        crate::migration::destination_validation::validate_layout(transaction.as_mut(), schema)
            .await?;
        validate_final_runtime_fence(transaction.as_mut(), schema).await
    }
    .await;
    let rollback = transaction
        .rollback()
        .await
        .map_err(|error| map_sql_error(schema, "finish runtime readiness validation", error));
    validation?;
    rollback
}

async fn validate_final_runtime_fence(
    connection: &mut PgConnection,
    schema: &str,
) -> anyhow::Result<()> {
    let migration = qualified_table(schema, "runtime_state_migration");
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT source_identity, source_fingerprint, phase, ready, phase_evidence, fencing_token \
         FROM {migration} WHERE singleton"
    )))
    .fetch_optional(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "read final Runtime State Migration fence", error))?
    .ok_or_else(|| runtime_not_ready(schema))?;
    let source_identity: String = row.try_get("source_identity").map_err(|error| {
        map_sql_error(schema, "decode final Runtime State Migration fence", error)
    })?;
    let source_fingerprint: String = row.try_get("source_fingerprint").map_err(|error| {
        map_sql_error(schema, "decode final Runtime State Migration fence", error)
    })?;
    let phase: String = row.try_get("phase").map_err(|error| {
        map_sql_error(schema, "decode final Runtime State Migration fence", error)
    })?;
    let ready: bool = row.try_get("ready").map_err(|error| {
        map_sql_error(schema, "decode final Runtime State Migration fence", error)
    })?;
    let evidence: Value = row.try_get("phase_evidence").map_err(|error| {
        map_sql_error(schema, "decode final Runtime State Migration fence", error)
    })?;
    let fencing_token: i64 = row.try_get("fencing_token").map_err(|error| {
        map_sql_error(schema, "decode final Runtime State Migration fence", error)
    })?;

    let evidence_matches_record = evidence.get("sourceIdentity").and_then(Value::as_str)
        == Some(source_identity.as_str())
        && evidence.get("sourceFingerprint").and_then(Value::as_str)
            == Some(source_fingerprint.as_str())
        && evidence.get("phase").and_then(Value::as_str) == Some("ready")
        && evidence.get("ready").and_then(Value::as_bool) == Some(true)
        && evidence.get("fencingToken").and_then(Value::as_i64) == Some(fencing_token)
        && evidence
            .get("namespaceDigest")
            .and_then(Value::as_str)
            .is_some_and(|digest| !digest.is_empty())
        && evidence
            .get("globalReferentialIntegrityValidated")
            .and_then(Value::as_bool)
            == Some(true)
        && evidence
            .get("canonicalThreadHistoryOrderingValidated")
            .and_then(Value::as_bool)
            == Some(true);
    let evidence_has_valid_origin = match evidence.get("initializationMode").and_then(Value::as_str)
    {
        Some("empty") => {
            source_identity == initialize::EMPTY_SOURCE_IDENTITY
                && source_fingerprint == initialize::EMPTY_SOURCE_FINGERPRINT
                && fencing_token == initialize::EMPTY_INITIALIZATION_FENCING_TOKEN
        }
        None => fencing_token == 4,
        Some(_) => false,
    };
    if phase != "ready" || !ready || !evidence_matches_record || !evidence_has_valid_origin {
        return Err(runtime_not_ready(schema));
    }
    Ok(())
}

fn runtime_not_ready(schema: &str) -> anyhow::Error {
    anyhow!(
        "PostgreSQL Runtime State Namespace `{schema}` is not ready for runtime use; complete `codex state migrate` for existing SQLite state or `codex state initialize` for a new empty namespace, then verify its readiness report before retrying"
    )
}
