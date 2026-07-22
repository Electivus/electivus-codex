use crate::postgres::map_sql_error;
use crate::postgres::qualified_table;
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
    anyhow::ensure!(
        !row.try_get::<bool, _>("ready")?,
        "PostgreSQL Runtime State Migration is already ready; retries are not allowed"
    );
    anyhow::ensure!(
        row.try_get::<String, _>("source_identity")? == source_identity
            && row.try_get::<String, _>("source_fingerprint")? == source_fingerprint,
        "PostgreSQL Runtime State Namespace belongs to a different migration source"
    );
    let evidence: serde_json::Value = row.try_get("phase_evidence")?;
    let expected_digest = evidence
        .get("namespaceDigest")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("PostgreSQL Runtime State Migration evidence is invalid"))?;
    anyhow::ensure!(
        namespace_digest(connection, schema).await? == expected_digest,
        "PostgreSQL Runtime State Namespace changed after its recorded migration phase"
    );
    Ok(Some(RuntimeStateMigrationProgress {
        phase: RuntimeStateMigrationPhase::parse(&row.try_get::<String, _>("phase")?)?,
        fencing_token: row.try_get("fencing_token")?,
    }))
}

pub(super) async fn namespace_digest(
    connection: &mut sqlx::PgConnection,
    schema: &str,
) -> anyhow::Result<String> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_catalog.pg_tables WHERE schemaname = $1 \
         AND tablename NOT IN ('_codex_runtime_state_migrations', 'runtime_state_migration') \
         ORDER BY tablename",
    )
    .bind(schema)
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "inventory Runtime State Migration evidence", error))?;
    let mut hasher = Sha256::new();
    for table in tables {
        let qualified = qualified_table(schema, &table);
        let rows: String = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(records) ORDER BY to_jsonb(records)::text), '[]'::jsonb)::text FROM {qualified} AS records"
        )))
        .fetch_one(&mut *connection)
        .await
        .map_err(|error| map_sql_error(schema, "read Runtime State Migration evidence", error))?;
        hash_field(&mut hasher, table.as_bytes());
        hash_field(&mut hasher, rows.as_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}
