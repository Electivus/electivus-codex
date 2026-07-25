use super::RuntimeStateMigrationInventory;
use super::RuntimeStateMigrationPhase;
use super::RuntimeStateMigrationProgress;
use super::import_threads::fingerprint;
use super::import_threads::revalidate_source;
use super::import_threads::source_identity;
use super::progress::RuntimeStateMigrationEvidence;
use super::progress::existing_progress;
use super::progress::namespace_digest;
use super::progress::phase_evidence;
use super::snapshot_memory::MemoryJobKind;
use super::snapshot_memory::MemoryJobSnapshot;
use super::snapshot_memory::MemoryMigrationSnapshot;
use super::snapshot_memory::MemoryStage1OutputSnapshot;
use super::snapshot_memory::hash_field;
use super::snapshot_memory::records_hash;
use super::snapshot_memory::snapshot_memory_state;
use crate::PostgresNamespaceAction;
use crate::PostgresNamespaceConfig;
use crate::SqliteConfig;
use crate::postgres::MAXIMUM_COMPATIBLE_SCHEMA_VERSION;
use crate::postgres::acquire_namespace_lock;
use crate::postgres::config::connect_pool;
use crate::postgres::manage_postgres_namespace_with_connection;
use crate::postgres::map_sql_error;
use crate::postgres::qualified_table;
use crate::runtime::import_migrated_memory_generation;
use anyhow::Context;
use futures::TryStreamExt;
use sha2::Digest;
use sha2::Sha256;
use sqlx::AssertSqlSafe;
use sqlx::Row;

type PostgresMemoryOutputRow = (
    String,
    i64,
    String,
    String,
    Option<String>,
    i64,
    Option<i64>,
    Option<i64>,
    bool,
    Option<i64>,
);
type PostgresMemoryJobRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

pub(super) struct MemoryMigrationEvidence {
    pub(super) outputs: i64,
    pub(super) jobs: i64,
    pub(super) used_outputs: i64,
    pub(super) selected_outputs: i64,
    pub(super) generations: i64,
    pub(super) artifacts: i64,
    pub(super) artifact_bytes: i64,
    pub(super) outputs_hash: String,
    pub(super) jobs_hash: String,
    pub(super) artifact_set_hash: String,
}

/// Revalidate and atomically import memory work state and one authoritative Memory Generation.
pub async fn import_runtime_state_memory(
    source: &SqliteConfig,
    destination: &PostgresNamespaceConfig,
    expected_inventory: &RuntimeStateMigrationInventory,
) -> anyhow::Result<RuntimeStateMigrationProgress> {
    anyhow::ensure!(
        expected_inventory.destination_schema == destination.schema()
            && expected_inventory.destination_schema_version == MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
        "Runtime State Migration inventory does not match the current PostgreSQL destination"
    );
    revalidate_source(source, expected_inventory).await?;
    let snapshot = snapshot_memory_state(source, expected_inventory).await?;
    revalidate_source(source, expected_inventory).await?;
    let source_identity = source_identity(source);
    let source_fingerprint = fingerprint(expected_inventory);
    let pool = connect_pool(destination).await?;
    revalidate_source(source, expected_inventory).await?;
    let result = import_snapshot(
        destination,
        &snapshot,
        &source_identity,
        &source_fingerprint,
        &pool,
    )
    .await;
    pool.close().await;
    result
}

async fn import_snapshot(
    destination: &PostgresNamespaceConfig,
    snapshot: &MemoryMigrationSnapshot,
    source_identity: &str,
    source_fingerprint: &str,
    pool: &sqlx::PgPool,
) -> anyhow::Result<RuntimeStateMigrationProgress> {
    let schema = destination.schema();
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin memory migration", error))?;
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
    .context("Runtime State Migration must import operational state before memory")?;
    match progress.phase {
        RuntimeStateMigrationPhase::OperationalImported => {}
        RuntimeStateMigrationPhase::MemoryImported => {
            transaction
                .commit()
                .await
                .map_err(|error| map_sql_error(schema, "finish memory migration retry", error))?;
            return Ok(progress);
        }
        RuntimeStateMigrationPhase::ThreadsImported => {
            anyhow::bail!("Runtime State Migration must import operational state before memory")
        }
        RuntimeStateMigrationPhase::Ready => {
            anyhow::bail!("Runtime State Migration is already ready; retries are not allowed")
        }
    }

    write_memory_state(transaction.as_mut(), schema, snapshot).await?;
    validate_imported_memory_state(transaction.as_mut(), schema, snapshot).await?;
    let generation = import_migrated_memory_generation(
        &mut transaction,
        pool.clone(),
        schema.to_string(),
        snapshot.completed_watermark(),
        &snapshot.artifacts,
    )
    .await
    .context("publish migrated Memory Generation")?;
    anyhow::ensure!(
        generation.completed_watermark() == snapshot.completed_watermark()
            && generation.artifacts() == snapshot.artifacts.artifacts(),
        "migrated Memory Generation does not match the SQLite memory workspace"
    );
    let migration = qualified_table(schema, "runtime_state_migration");
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {migration} SET phase = 'memory_imported', ready = FALSE, \
         phase_evidence = '{{}}'::jsonb, fencing_token = 3, updated_at = CURRENT_TIMESTAMP \
         WHERE singleton"
    )))
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "record memory migration phase", error))?;
    let digest = namespace_digest(transaction.as_mut(), schema).await?;
    let evidence = phase_evidence(
        transaction.as_mut(),
        schema,
        RuntimeStateMigrationEvidence {
            source_identity,
            source_fingerprint,
            phase: RuntimeStateMigrationPhase::MemoryImported,
            ready: false,
            fencing_token: 3,
            namespace_digest: &digest,
        },
    )
    .await?;
    anyhow::ensure!(
        evidence["memoryStage1OutputsHash"]
            == serde_json::Value::String(records_hash(&snapshot.outputs)?)
            && evidence["memoryJobsHash"]
                == serde_json::Value::String(records_hash(&snapshot.jobs)?)
            && evidence["memoryArtifactSetHash"]
                == serde_json::Value::String(snapshot.artifact_set_hash()),
        "migrated memory evidence does not match the SQLite source"
    );
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {migration} SET phase_evidence = $1 WHERE singleton"
    )))
    .bind(evidence)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "seal memory migration evidence", error))?;
    transaction
        .commit()
        .await
        .map_err(|error| map_sql_error(schema, "commit memory migration", error))?;
    Ok(RuntimeStateMigrationProgress {
        phase: RuntimeStateMigrationPhase::MemoryImported,
        fencing_token: 3,
    })
}

async fn validate_imported_memory_state(
    connection: &mut sqlx::PgConnection,
    schema: &str,
    snapshot: &MemoryMigrationSnapshot,
) -> anyhow::Result<()> {
    let (imported_outputs, imported_jobs) = read_imported_memory_state(connection, schema).await?;
    anyhow::ensure!(
        imported_outputs.as_slice() == snapshot.outputs.as_slice(),
        "migrated stage-one memory outputs do not match SQLite"
    );
    anyhow::ensure!(
        imported_jobs.as_slice() == snapshot.jobs.as_slice(),
        "migrated memory jobs do not match SQLite"
    );
    Ok(())
}

async fn read_imported_memory_state(
    connection: &mut sqlx::PgConnection,
    schema: &str,
) -> anyhow::Result<(Vec<MemoryStage1OutputSnapshot>, Vec<MemoryJobSnapshot>)> {
    let outputs = qualified_table(schema, "memory_stage1_outputs");
    let imported_outputs = sqlx::query_as::<_, PostgresMemoryOutputRow>(AssertSqlSafe(format!(
        "SELECT thread_id, source_updated_at, raw_memory, rollout_summary, rollout_slug, \
         generated_at, usage_count, last_usage, selected_for_phase2, \
         selected_for_phase2_source_updated_at FROM {outputs} ORDER BY thread_id"
    )))
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "validate stage-one memory outputs", error))?
    .into_iter()
    .map(output_from_postgres_row)
    .collect();
    let jobs = qualified_table(schema, "memory_jobs");
    let imported_jobs = sqlx::query_as::<_, PostgresMemoryJobRow>(AssertSqlSafe(format!(
        "SELECT kind, job_key, status, worker_id, ownership_token, started_at, finished_at, \
         lease_until, retry_at, retry_remaining, last_error, input_watermark, \
         last_success_watermark FROM {jobs} ORDER BY kind, job_key"
    )))
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "validate memory jobs", error))?
    .into_iter()
    .map(job_from_postgres_row)
    .collect::<anyhow::Result<Vec<_>>>()?;
    Ok((imported_outputs, imported_jobs))
}

fn output_from_postgres_row(row: PostgresMemoryOutputRow) -> MemoryStage1OutputSnapshot {
    MemoryStage1OutputSnapshot {
        thread_id: row.0,
        source_updated_at: row.1,
        raw_memory: row.2,
        rollout_summary: row.3,
        rollout_slug: row.4,
        generated_at: row.5,
        usage_count: row.6,
        last_usage: row.7,
        selected_for_phase2: row.8,
        selected_for_phase2_source_updated_at: row.9,
    }
}

fn job_from_postgres_row(row: PostgresMemoryJobRow) -> anyhow::Result<MemoryJobSnapshot> {
    Ok(MemoryJobSnapshot {
        kind: MemoryJobKind::parse(&row.0)?,
        job_key: row.1,
        status: super::snapshot_memory::MemoryJobStatus::parse(&row.2)?,
        worker_id: row.3,
        ownership_token: row.4,
        started_at: row.5,
        finished_at: row.6,
        lease_until: row.7,
        retry_at: row.8,
        retry_remaining: row.9,
        last_error: row.10,
        input_watermark: row.11,
        last_success_watermark: row.12,
    })
}

pub(super) async fn memory_evidence(
    connection: &mut sqlx::PgConnection,
    schema: &str,
) -> anyhow::Result<MemoryMigrationEvidence> {
    let (outputs, jobs) = read_imported_memory_state(connection, schema).await?;
    let used_outputs = outputs
        .iter()
        .filter(|output| output.usage_count.is_some() || output.last_usage.is_some())
        .count();
    let selected_outputs = outputs
        .iter()
        .filter(|output| {
            output.selected_for_phase2 || output.selected_for_phase2_source_updated_at.is_some()
        })
        .count();
    let generations = qualified_table(schema, "memory_generations");
    let artifacts = qualified_table(schema, "memory_generation_artifacts");
    let generation_counts: (i64, i64, i64) = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT (SELECT COUNT(*) FROM {generations}), (SELECT COUNT(*) FROM {artifacts}), \
         (SELECT COALESCE(SUM(octet_length(contents)), 0)::bigint FROM {artifacts})"
    )))
    .fetch_one(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "validate memory migration evidence", error))?;
    let state = qualified_table(schema, "memory_generation_state");
    let mut rows = sqlx::query(AssertSqlSafe(format!(
        "SELECT artifact.artifact_path, sha256(artifact.contents) AS contents_hash FROM {state} \
         AS state JOIN {artifacts} AS artifact \
         ON artifact.generation_id = state.active_generation_id WHERE state.singleton \
         ORDER BY artifact.artifact_path COLLATE \"C\""
    )))
    .fetch(&mut *connection);
    let mut artifact_hasher = Sha256::new();
    while let Some(row) = rows
        .try_next()
        .await
        .map_err(|error| map_sql_error(schema, "hash migrated Memory Artifacts", error))?
    {
        let path: String = row.try_get("artifact_path")?;
        let contents_hash: Vec<u8> = row.try_get("contents_hash")?;
        hash_field(&mut artifact_hasher, path.as_bytes());
        hash_field(&mut artifact_hasher, &contents_hash);
    }
    Ok(MemoryMigrationEvidence {
        outputs: i64::try_from(outputs.len())?,
        jobs: i64::try_from(jobs.len())?,
        used_outputs: i64::try_from(used_outputs)?,
        selected_outputs: i64::try_from(selected_outputs)?,
        generations: generation_counts.0,
        artifacts: generation_counts.1,
        artifact_bytes: generation_counts.2,
        outputs_hash: records_hash(&outputs)?,
        jobs_hash: records_hash(&jobs)?,
        artifact_set_hash: format!("{:x}", artifact_hasher.finalize()),
    })
}

async fn write_memory_state(
    connection: &mut sqlx::PgConnection,
    schema: &str,
    snapshot: &MemoryMigrationSnapshot,
) -> anyhow::Result<()> {
    let outputs = qualified_table(schema, "memory_stage1_outputs");
    for row in &snapshot.outputs {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {outputs} (thread_id, source_updated_at, raw_memory, rollout_summary, \
             rollout_slug, generated_at, usage_count, last_usage, selected_for_phase2, \
             selected_for_phase2_source_updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"
        )))
        .bind(&row.thread_id)
        .bind(row.source_updated_at)
        .bind(&row.raw_memory)
        .bind(&row.rollout_summary)
        .bind(&row.rollout_slug)
        .bind(row.generated_at)
        .bind(row.usage_count)
        .bind(row.last_usage)
        .bind(row.selected_for_phase2)
        .bind(row.selected_for_phase2_source_updated_at)
        .execute(&mut *connection)
        .await
        .map_err(|error| map_sql_error(schema, "import stage-one memory outputs", error))?;
    }
    let jobs = qualified_table(schema, "memory_jobs");
    for row in &snapshot.jobs {
        write_memory_job(connection, schema, &jobs, row).await?;
    }
    Ok(())
}

async fn write_memory_job(
    connection: &mut sqlx::PgConnection,
    schema: &str,
    jobs: &str,
    row: &MemoryJobSnapshot,
) -> anyhow::Result<()> {
    let thread_id = match row.kind {
        MemoryJobKind::MemoryStage1 => Some(row.job_key.as_str()),
        MemoryJobKind::MemoryConsolidateGlobal => None,
    };
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {jobs} (kind, job_key, thread_id, status, worker_id, ownership_token, \
         started_at, finished_at, lease_until, retry_at, retry_remaining, last_error, \
         input_watermark, last_success_watermark) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)"
    )))
    .bind(row.kind.as_str())
    .bind(&row.job_key)
    .bind(thread_id)
    .bind(row.status.as_str())
    .bind(&row.worker_id)
    .bind(&row.ownership_token)
    .bind(row.started_at)
    .bind(row.finished_at)
    .bind(row.lease_until)
    .bind(row.retry_at)
    .bind(row.retry_remaining)
    .bind(&row.last_error)
    .bind(row.input_watermark)
    .bind(row.last_success_watermark)
    .execute(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "import memory jobs", error))?;
    Ok(())
}
