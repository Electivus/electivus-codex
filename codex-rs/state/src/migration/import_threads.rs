use super::CanonicalThreadHistoryReader;
use super::RuntimeStateMigrationInventory;
use super::RuntimeStateMigrationPhase;
use super::RuntimeStateMigrationProgress;
use super::RuntimeStateThreadProjectionMaterializer;
use super::RuntimeStateThreadSnapshot;
use super::ThreadMigrationSnapshot;
use super::destination_validation;
use super::inspect_source;
use super::progress::existing_progress;
use super::progress::namespace_digest;
use super::snapshot_runtime_state_migration_threads;
use super::thread_evidence::snapshot_content_evidence;
use super::thread_evidence::thread_content_evidence;
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
use chrono::DateTime;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::ThreadHistoryMode;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use sqlx::AssertSqlSafe;

/// Revalidate and atomically import the authoritative local thread domain into PostgreSQL.
pub async fn import_runtime_state_threads(
    source: &SqliteConfig,
    destination: &PostgresNamespaceConfig,
    expected_inventory: &RuntimeStateMigrationInventory,
    history_reader: &impl CanonicalThreadHistoryReader,
    projection_materializer: &impl RuntimeStateThreadProjectionMaterializer,
) -> anyhow::Result<RuntimeStateMigrationProgress> {
    anyhow::ensure!(
        projection_materializer.destination_schema() == destination.schema(),
        "Runtime State Migration projection materializer targets PostgreSQL schema `{}` instead of destination schema `{}`",
        projection_materializer.destination_schema(),
        destination.schema()
    );
    anyhow::ensure!(
        expected_inventory.destination_schema == destination.schema()
            && expected_inventory.destination_schema_version == MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
        "Runtime State Migration inventory does not match the current PostgreSQL destination"
    );
    revalidate_source(source, expected_inventory).await?;
    let snapshot =
        snapshot_runtime_state_migration_threads(source, expected_inventory, history_reader)
            .await?;
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
        projection_materializer,
    )
    .await;
    pool.close().await;
    result
}

pub(super) async fn revalidate_source(
    source: &SqliteConfig,
    expected: &RuntimeStateMigrationInventory,
) -> anyhow::Result<()> {
    let actual = inspect_source(source).await?;
    anyhow::ensure!(
        actual.databases == expected.databases
            && actual.rollout_files == expected.rollout_files
            && actual.memory_files == expected.memory_files
            && actual.imported_resources == expected.imported_resources
            && actual.configuration == expected.configuration
            && actual.session_index == expected.session_index,
        "Runtime State Migration source changed after preflight; stop every process using it and retry"
    );
    Ok(())
}

async fn import_snapshot(
    destination: &PostgresNamespaceConfig,
    snapshot: &RuntimeStateThreadSnapshot,
    source_identity: &str,
    source_fingerprint: &str,
    pool: &sqlx::PgPool,
    projection_materializer: &impl RuntimeStateThreadProjectionMaterializer,
) -> anyhow::Result<RuntimeStateMigrationProgress> {
    let schema = destination.schema();
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin thread migration", error))?;
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
    if let Some(progress) = existing_progress(
        transaction.as_mut(),
        schema,
        source_identity,
        source_fingerprint,
    )
    .await?
    {
        transaction
            .commit()
            .await
            .map_err(|error| map_sql_error(schema, "finish thread migration retry", error))?;
        return Ok(progress);
    }
    destination_validation::ensure_empty(transaction.as_mut(), schema).await?;
    write_threads(transaction.as_mut(), schema, snapshot).await?;
    projection_materializer
        .materialize(transaction.as_mut(), snapshot)
        .await
        .map_err(anyhow::Error::new)?;
    let expected_content = snapshot_content_evidence(snapshot)?;
    let content = thread_content_evidence(transaction.as_mut(), schema).await?;
    anyhow::ensure!(
        content == expected_content,
        "imported thread identifiers, content, or Canonical Thread History ordering do not exactly match the SQLite source snapshot"
    );
    let migration = qualified_table(schema, "runtime_state_migration");
    let mut evidence = json!({
        "sourceIdentity": source_identity,
        "threads": snapshot.threads.len(),
        "historyLines": snapshot
            .threads
            .iter()
            .map(|thread| thread.canonical_history.lines().len())
            .sum::<usize>(),
        "threadsContentHash": content.threads_hash,
        "historyContentHash": content.history_hash,
        "threadCoordinationContentHash": content.coordination_hash,
        "sourceFingerprint": source_fingerprint,
        "phase": "threads_imported",
        "ready": false,
        "fencingToken": 1,
    });
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {migration} (source_identity, source_fingerprint, phase, ready, phase_evidence, fencing_token) \
         VALUES ($1, $2, 'threads_imported', FALSE, $3, 1)"
    )))
    .bind(source_identity)
    .bind(source_fingerprint)
    .bind(&evidence)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "record thread migration phase", error))?;
    let namespace_digest = namespace_digest(transaction.as_mut(), schema).await?;
    evidence["namespaceDigest"] = Value::String(namespace_digest);
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {migration} SET phase_evidence = $1 WHERE singleton"
    )))
    .bind(evidence)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "seal thread migration evidence", error))?;
    transaction
        .commit()
        .await
        .map_err(|error| map_sql_error(schema, "commit thread migration", error))?;
    Ok(RuntimeStateMigrationProgress {
        phase: RuntimeStateMigrationPhase::ThreadsImported,
        fencing_token: 1,
    })
}

async fn write_threads(
    connection: &mut sqlx::PgConnection,
    schema: &str,
    snapshot: &RuntimeStateThreadSnapshot,
) -> anyhow::Result<()> {
    for thread in &snapshot.threads {
        write_thread(connection, schema, thread).await?;
    }
    let edges = qualified_table(schema, "thread_spawn_edges");
    for edge in &snapshot.spawn_edges {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {edges} (parent_thread_id, child_thread_id, status) VALUES ($1, $2, $3)"
        )))
        .bind(edge.parent_thread_id.to_string())
        .bind(edge.child_thread_id.to_string())
        .bind(edge.status.as_ref())
        .execute(&mut *connection)
        .await
        .map_err(|error| map_sql_error(schema, "import thread lineage", error))?;
    }
    let backfill = qualified_table(schema, "backfill_state");
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {backfill} SET status = $1, last_watermark = $2, last_success_at = $3, \
         updated_at = $4, owner_id = $5, fencing_token = $6, lease_expires_at = $7 WHERE id = 1"
    )))
    .bind(snapshot.backfill.state.status.as_str())
    .bind(&snapshot.backfill.state.last_watermark)
    .bind(snapshot.backfill.state.last_success_at)
    .bind(snapshot.backfill.updated_at)
    .bind(&snapshot.backfill.owner_id)
    .bind(snapshot.backfill.fencing_token)
    .bind(snapshot.backfill.lease_expires_at)
    .execute(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "import backfill coordination", error))?;
    Ok(())
}

async fn write_thread(
    connection: &mut sqlx::PgConnection,
    schema: &str,
    thread: &ThreadMigrationSnapshot,
) -> anyhow::Result<()> {
    let metadata = &thread.metadata;
    let session_meta = thread
        .canonical_history
        .lines()
        .iter()
        .find_map(|line| match &line.item {
            RolloutItem::SessionMeta(meta) => Some(meta),
            _ => None,
        })
        .context("Canonical Thread History has no SessionMeta")?;
    let projection = thread_projection(thread, session_meta)?;
    let stream_version = i64::try_from(thread.canonical_history.lines().len())
        .context("Canonical Thread History is too large")?;
    let projection_start = session_meta
        .meta
        .subagent_history_start_ordinal
        .map(i64::try_from)
        .transpose()
        .context("thread projection start is too large")?;
    let threads = qualified_table(schema, "threads");
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {threads} (thread_id, projection, stream_version, history_projection_version, \
         history_projection_start_ordinal, fencing_token, writer_id, writer_lease_expires_at, \
         created_at, updated_at, recency_at, archived_at) \
         VALUES ($1, $2, $3, NULL, $4, 1, 'runtime-state-migration', '-infinity', $5, $6, $7, $8)"
    )))
    .bind(metadata.id.to_string())
    .bind(projection)
    .bind(stream_version)
    .bind(projection_start)
    .bind(metadata.created_at)
    .bind(metadata.updated_at)
    .bind(metadata.recency_at)
    .bind(metadata.archived_at)
    .execute(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "import thread metadata", error))?;
    if let Some(polluted_at_stream_version) = thread.polluted_at_stream_version {
        let overrides = qualified_table(schema, "memory_thread_mode_overrides");
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {overrides} (thread_id, polluted_at_stream_version) VALUES ($1, $2)"
        )))
        .bind(metadata.id.to_string())
        .bind(polluted_at_stream_version)
        .execute(&mut *connection)
        .await
        .map_err(|error| map_sql_error(schema, "import thread memory pollution", error))?;
    }
    write_history(connection, schema, thread).await?;
    Ok(())
}

pub(super) fn thread_projection(
    thread: &ThreadMigrationSnapshot,
    session_meta: &codex_protocol::protocol::SessionMetaLine,
) -> anyhow::Result<Value> {
    let metadata = &thread.metadata;
    let name = match metadata.history_mode {
        ThreadHistoryMode::Legacy
            if !metadata.title.trim().is_empty()
                && metadata.first_user_message.as_deref().map(str::trim)
                    != Some(metadata.title.trim()) =>
        {
            Some(metadata.title.trim().to_string())
        }
        ThreadHistoryMode::Legacy | ThreadHistoryMode::Paginated => metadata.name.clone(),
    };
    let git_info = (metadata.git_sha.is_some()
        || metadata.git_branch.is_some()
        || metadata.git_origin_url.is_some())
    .then(|| {
        json!({
            "commit_hash": metadata.git_sha,
            "branch": metadata.git_branch,
            "repository_url": metadata.git_origin_url,
        })
    });
    let source = serde_json::from_str(&metadata.source)
        .unwrap_or_else(|_| Value::String(metadata.source.clone()));
    let approval_mode = serde_json::from_str(&metadata.approval_mode)
        .unwrap_or_else(|_| Value::String(metadata.approval_mode.clone()));
    let permission_profile = serde_json::from_str::<PermissionProfile>(&metadata.sandbox_policy)
        .or_else(|_| {
            serde_json::from_str::<SandboxPolicy>(&metadata.sandbox_policy).map(|policy| {
                PermissionProfile::from_legacy_sandbox_policy_for_cwd(&policy, &metadata.cwd)
            })
        })
        .unwrap_or_else(|_| PermissionProfile::read_only());
    Ok(json!({
        "thread_id": metadata.id,
        "extra_config": null,
        "rollout_path": null,
        "forked_from_id": session_meta.meta.forked_from_id,
        "parent_thread_id": session_meta.meta.parent_thread_id,
        "preview": metadata.preview.clone().or_else(|| metadata.first_user_message.clone()).unwrap_or_default(),
        "name": name,
        "model_provider": metadata.model_provider,
        "model": metadata.model,
        "reasoning_effort": metadata.reasoning_effort,
        "created_at": metadata.created_at,
        "updated_at": metadata.updated_at,
        "recency_at": metadata.recency_at,
        "archived_at": metadata.archived_at,
        "cwd": metadata.cwd,
        "cli_version": metadata.cli_version,
        "source": source,
        "history_mode": metadata.history_mode,
        "thread_source": metadata.thread_source,
        "agent_nickname": metadata.agent_nickname,
        "agent_role": metadata.agent_role,
        "agent_path": metadata.agent_path,
        "git_info": git_info,
        "approval_mode": approval_mode,
        "permission_profile": permission_profile,
        "token_usage": (metadata.tokens_used != 0).then(|| json!({
            "input_tokens": 0,
            "cached_input_tokens": 0,
            "cache_write_input_tokens": 0,
            "output_tokens": 0,
            "reasoning_output_tokens": 0,
            "total_tokens": metadata.tokens_used,
        })),
        "first_user_message": metadata.first_user_message,
        "history": null,
        "memory_mode": thread.memory_mode,
        "dynamic_tools": thread.dynamic_tools,
    }))
}

async fn write_history(
    connection: &mut sqlx::PgConnection,
    schema: &str,
    thread: &ThreadMigrationSnapshot,
) -> anyhow::Result<()> {
    let history = qualified_table(schema, "thread_history");
    for (ordinal, line) in thread.canonical_history.lines().iter().enumerate() {
        let ordinal = i64::try_from(ordinal).context("Canonical Thread History is too large")?;
        let source_ordinal = line
            .ordinal
            .map(i64::try_from)
            .transpose()
            .context("source rollout ordinal is too large")?;
        let recorded_at = DateTime::parse_from_rfc3339(&line.timestamp)
            .context("invalid Canonical Thread History timestamp")?
            .to_utc();
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {history} (thread_id, ordinal, source_ordinal, item, recorded_at) \
             VALUES ($1, $2, $3, $4, $5)"
        )))
        .bind(thread.metadata.id.to_string())
        .bind(ordinal)
        .bind(source_ordinal)
        .bind(serde_json::to_value(&line.item)?)
        .bind(recorded_at)
        .execute(&mut *connection)
        .await
        .map_err(|error| map_sql_error(schema, "import Canonical Thread History", error))?;
    }
    Ok(())
}

pub(super) fn fingerprint(inventory: &RuntimeStateMigrationInventory) -> String {
    let mut hasher = Sha256::new();
    for database in &inventory.databases {
        hash_field(&mut hasher, database.label.as_bytes());
        hash_file(&mut hasher, &database.file);
        for table in &database.tables {
            hash_field(&mut hasher, table.name.as_bytes());
            hash_field(&mut hasher, &table.row_count.to_be_bytes());
        }
    }
    for files in [
        &inventory.rollout_files,
        &inventory.memory_files,
        &inventory.imported_resources,
    ] {
        for file in files {
            hash_file(&mut hasher, file);
        }
    }
    if let Some(configuration) = &inventory.configuration {
        hash_field(&mut hasher, b"configuration");
        hash_file(&mut hasher, configuration);
    } else {
        hash_field(&mut hasher, b"no-configuration");
    }
    if let Some(session_index) = &inventory.session_index {
        hash_field(&mut hasher, b"session-index");
        hash_file(&mut hasher, session_index);
    } else {
        hash_field(&mut hasher, b"no-session-index");
    }
    format!("{:x}", hasher.finalize())
}

fn hash_file(hasher: &mut Sha256, file: &super::SourceFileInventory) {
    hash_field(hasher, file.relative_path.as_os_str().as_encoded_bytes());
    hash_field(hasher, &file.size_bytes.to_be_bytes());
    hash_field(hasher, file.sha256.as_bytes());
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}

pub(super) fn source_identity(source: &SqliteConfig) -> String {
    format!(
        "{:x}",
        Sha256::digest(source.home().as_os_str().as_encoded_bytes())
    )
}
