use crate::postgres::map_sql_error;
use crate::postgres::qualified_table;
use futures::TryStreamExt;
use sha2::Digest;
use sha2::Sha256;
use sqlx::AssertSqlSafe;
use sqlx::Row;

/// Durable phase reached by an explicit Runtime State Migration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeStateMigrationPhase {
    ThreadsImported,
    OperationalImported,
    MemoryImported,
    Ready,
}

impl RuntimeStateMigrationPhase {
    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "threads_imported" => Ok(Self::ThreadsImported),
            "operational_imported" => Ok(Self::OperationalImported),
            "memory_imported" => Ok(Self::MemoryImported),
            "ready" => Ok(Self::Ready),
            _ => anyhow::bail!("PostgreSQL Runtime State Migration has an invalid phase"),
        }
    }
}

/// Durable migration position after an idempotent phase attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeStateMigrationProgress {
    pub(super) phase: RuntimeStateMigrationPhase,
    pub(super) fencing_token: i64,
}

impl RuntimeStateMigrationProgress {
    pub fn phase(&self) -> RuntimeStateMigrationPhase {
        self.phase
    }

    pub fn fencing_token(&self) -> i64 {
        self.fencing_token
    }
}

pub(super) async fn existing_progress(
    connection: &mut sqlx::PgConnection,
    schema: &str,
    source_identity: &str,
    source_fingerprint: &str,
) -> anyhow::Result<Option<RuntimeStateMigrationProgress>> {
    let migration = qualified_table(schema, "runtime_state_migration");
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT source_identity, source_fingerprint, phase, ready, phase_evidence, fencing_token \
         FROM {migration} WHERE singleton FOR UPDATE"
    )))
    .fetch_optional(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "read Runtime State Migration phase", error))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let recorded_source_identity: String = row.try_get("source_identity")?;
    let recorded_source_fingerprint: String = row.try_get("source_fingerprint")?;
    let phase: String = row.try_get("phase")?;
    let ready: bool = row.try_get("ready")?;
    let fencing_token: i64 = row.try_get("fencing_token")?;
    anyhow::ensure!(
        recorded_source_identity == source_identity
            && recorded_source_fingerprint == source_fingerprint,
        "PostgreSQL Runtime State Namespace belongs to a different migration source"
    );
    let evidence: serde_json::Value = row.try_get("phase_evidence")?;
    anyhow::ensure!(
        !ready,
        "PostgreSQL Runtime State Migration is already ready; retries are not allowed"
    );
    let expected_digest = evidence
        .get("namespaceDigest")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("PostgreSQL Runtime State Migration evidence is invalid"))?;
    let threads = qualified_table(schema, "threads");
    let history = qualified_table(schema, "thread_history");
    let counts: (i64, i64) = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT (SELECT COUNT(*) FROM {threads}), (SELECT COUNT(*) FROM {history})"
    )))
    .fetch_one(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "validate Runtime State Migration evidence", error))?;
    let expected_evidence = serde_json::json!({
        "sourceIdentity": recorded_source_identity,
        "sourceFingerprint": recorded_source_fingerprint,
        "phase": phase,
        "ready": ready,
        "fencingToken": fencing_token,
        "threads": counts.0,
        "historyLines": counts.1,
        "namespaceDigest": expected_digest,
    });
    anyhow::ensure!(
        evidence == expected_evidence,
        "PostgreSQL Runtime State Migration control evidence changed after its recorded phase"
    );
    anyhow::ensure!(
        namespace_digest(connection, schema).await? == expected_digest,
        "PostgreSQL Runtime State Namespace changed after its recorded migration phase"
    );
    Ok(Some(RuntimeStateMigrationProgress {
        phase: RuntimeStateMigrationPhase::parse(&phase)?,
        fencing_token,
    }))
}

/// Hash every namespace row in a deterministic order without aggregating a table client-side.
///
/// Client memory is bounded by one PostgreSQL row serialized as JSONB text. A single unusually
/// large row still requires memory proportional to that row, and PostgreSQL may use `work_mem` or
/// spill to disk while ordering rows; the digest never builds a whole-table JSON value.
pub(super) async fn namespace_digest(
    connection: &mut sqlx::PgConnection,
    schema: &str,
) -> anyhow::Result<String> {
    sqlx::query(
        "SELECT set_config('TimeZone', 'UTC', true), \
         set_config('DateStyle', 'ISO, YMD', true), \
         set_config('bytea_output', 'hex', true), \
         set_config('extra_float_digits', '3', true), \
         set_config('lc_numeric', 'C', true)",
    )
    .execute(&mut *connection)
    .await
    .map_err(|error| {
        map_sql_error(
            schema,
            "canonicalize Runtime State Migration evidence",
            error,
        )
    })?;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_catalog.pg_tables WHERE schemaname = $1 \
         AND tablename <> '_codex_runtime_state_migrations' \
         ORDER BY tablename",
    )
    .bind(schema)
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "inventory Runtime State Migration evidence", error))?;
    let mut hasher = Sha256::new();
    for table in tables {
        let qualified = qualified_table(schema, &table);
        hash_field(&mut hasher, table.as_bytes());
        let record = if table == "runtime_state_migration" {
            "(to_jsonb(records) - 'phase_evidence')::text"
        } else {
            "to_jsonb(records)::text"
        };
        let mut rows = sqlx::query_scalar::<_, String>(AssertSqlSafe(format!(
            "SELECT record FROM (SELECT {record} AS record FROM {qualified} AS records) \
             AS canonical_records ORDER BY record COLLATE \"C\""
        )))
        .fetch(&mut *connection);
        while let Some(row) = rows.try_next().await.map_err(|error| {
            map_sql_error(schema, "read Runtime State Migration evidence", error)
        })? {
            hash_field(&mut hasher, row.as_bytes());
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}
