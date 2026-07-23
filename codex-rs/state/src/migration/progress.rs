use super::import_memory::memory_evidence;
use super::thread_evidence::thread_content_evidence;
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

    fn as_str(self) -> &'static str {
        match self {
            Self::ThreadsImported => "threads_imported",
            Self::OperationalImported => "operational_imported",
            Self::MemoryImported => "memory_imported",
            Self::Ready => "ready",
        }
    }
}

pub(crate) struct RuntimeStateMigrationEvidence<'a> {
    pub(crate) source_identity: &'a str,
    pub(crate) source_fingerprint: &'a str,
    pub(crate) phase: RuntimeStateMigrationPhase,
    pub(crate) ready: bool,
    pub(crate) fencing_token: i64,
    pub(crate) namespace_digest: &'a str,
}

/// Durable migration position after an idempotent phase attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeStateMigrationProgress {
    pub(super) phase: RuntimeStateMigrationPhase,
    pub(super) fencing_token: i64,
}

enum ProgressRead {
    Inspect,
    Lock,
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
    read_progress(
        connection,
        schema,
        source_identity,
        source_fingerprint,
        ProgressRead::Lock,
    )
    .await
}

pub(super) async fn inspect_existing_progress(
    connection: &mut sqlx::PgConnection,
    schema: &str,
    source_identity: &str,
    source_fingerprint: &str,
) -> anyhow::Result<Option<RuntimeStateMigrationProgress>> {
    read_progress(
        connection,
        schema,
        source_identity,
        source_fingerprint,
        ProgressRead::Inspect,
    )
    .await
}

async fn read_progress(
    connection: &mut sqlx::PgConnection,
    schema: &str,
    source_identity: &str,
    source_fingerprint: &str,
    read: ProgressRead,
) -> anyhow::Result<Option<RuntimeStateMigrationProgress>> {
    let migration = qualified_table(schema, "runtime_state_migration");
    let lock = match read {
        ProgressRead::Inspect => "",
        ProgressRead::Lock => " FOR UPDATE",
    };
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT source_identity, source_fingerprint, phase, ready, phase_evidence, fencing_token \
         FROM {migration} WHERE singleton{lock}"
    )))
    .fetch_optional(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "read Runtime State Migration phase", error))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let recorded_source_identity: String = row.try_get("source_identity")?;
    let recorded_source_fingerprint: String = row.try_get("source_fingerprint")?;
    let phase = RuntimeStateMigrationPhase::parse(row.try_get::<String, _>("phase")?.as_str())?;
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
    let expected_evidence = phase_evidence(
        connection,
        schema,
        RuntimeStateMigrationEvidence {
            source_identity: &recorded_source_identity,
            source_fingerprint: &recorded_source_fingerprint,
            phase,
            ready,
            fencing_token,
            namespace_digest: expected_digest,
        },
    )
    .await?;
    anyhow::ensure!(
        evidence == expected_evidence,
        "PostgreSQL Runtime State Migration control evidence changed after its recorded phase"
    );
    anyhow::ensure!(
        namespace_digest(connection, schema).await? == expected_digest,
        "PostgreSQL Runtime State Namespace changed after its recorded migration phase"
    );
    Ok(Some(RuntimeStateMigrationProgress {
        phase,
        fencing_token,
    }))
}

pub(crate) async fn phase_evidence(
    connection: &mut sqlx::PgConnection,
    schema: &str,
    metadata: RuntimeStateMigrationEvidence<'_>,
) -> anyhow::Result<serde_json::Value> {
    let threads = qualified_table(schema, "threads");
    let history = qualified_table(schema, "thread_history");
    let counts: (i64, i64) = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT (SELECT COUNT(*) FROM {threads}), (SELECT COUNT(*) FROM {history})"
    )))
    .fetch_one(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "validate Runtime State Migration evidence", error))?;
    let thread_content = thread_content_evidence(connection, schema).await?;
    let mut evidence = serde_json::json!({
        "sourceIdentity": metadata.source_identity,
        "sourceFingerprint": metadata.source_fingerprint,
        "phase": metadata.phase.as_str(),
        "ready": metadata.ready,
        "fencingToken": metadata.fencing_token,
        "threads": counts.0,
        "historyLines": counts.1,
        "threadsContentHash": thread_content.threads_hash,
        "historyContentHash": thread_content.history_hash,
        "threadCoordinationContentHash": thread_content.coordination_hash,
        "namespaceDigest": metadata.namespace_digest,
    });
    if metadata.phase != RuntimeStateMigrationPhase::ThreadsImported {
        let tables = [
            ("logs", "logs"),
            ("goals", "thread_goals"),
            ("goalDeferrals", "thread_goal_continuation_deferrals"),
            ("goalAccountingEvents", "thread_goal_accounting_events"),
            ("remoteControlEnrollments", "remote_control_enrollments"),
            (
                "externalAgentConfigImports",
                "external_agent_config_imports",
            ),
        ];
        for (field, table) in tables {
            let table = qualified_table(schema, table);
            let count: i64 =
                sqlx::query_scalar(AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                    .fetch_one(&mut *connection)
                    .await
                    .map_err(|error| {
                        map_sql_error(schema, "validate operational migration evidence", error)
                    })?;
            evidence[field] = serde_json::Value::from(count);
        }
    }
    if matches!(
        metadata.phase,
        RuntimeStateMigrationPhase::MemoryImported | RuntimeStateMigrationPhase::Ready
    ) {
        add_memory_evidence(connection, schema, &mut evidence).await?;
    }
    Ok(evidence)
}

async fn add_memory_evidence(
    connection: &mut sqlx::PgConnection,
    schema: &str,
    evidence: &mut serde_json::Value,
) -> anyhow::Result<()> {
    let memory = memory_evidence(connection, schema).await?;
    for (field, value) in [
        ("memoryStage1Outputs", memory.outputs),
        ("memoryJobs", memory.jobs),
        ("memoryUsedOutputs", memory.used_outputs),
        ("memorySelectedOutputs", memory.selected_outputs),
        ("memoryGenerations", memory.generations),
        ("memoryArtifacts", memory.artifacts),
        ("memoryArtifactBytes", memory.artifact_bytes),
    ] {
        evidence[field] = serde_json::Value::from(value);
    }
    evidence["memoryStage1OutputsHash"] = serde_json::Value::String(memory.outputs_hash);
    evidence["memoryJobsHash"] = serde_json::Value::String(memory.jobs_hash);
    evidence["memoryArtifactSetHash"] = serde_json::Value::String(memory.artifact_set_hash);
    Ok(())
}

/// Hash every namespace row in a deterministic order without aggregating a table client-side.
///
/// Client memory is bounded by one PostgreSQL row serialized as JSONB text. A single unusually
/// large row still requires memory proportional to that row, and PostgreSQL may use `work_mem` or
/// spill to disk while ordering rows; the digest never builds a whole-table JSON value.
pub(crate) async fn namespace_digest(
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
