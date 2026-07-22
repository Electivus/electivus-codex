use super::RuntimeStateMigrationInventory;
use super::source_validation;
use crate::BackfillState;
use crate::BackfillStatus;
use crate::DirectionalThreadSpawnEdgeStatus;
use crate::ExtractionOutcome;
use crate::SqliteConfig;
use crate::ThreadMetadata;
use crate::model::ThreadRow;
use crate::state_db_path;
use crate::thread_history_db_path;
use anyhow::Context;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::dynamic_tools::group_dynamic_tools_by_namespace;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::ThreadMemoryMode;
use serde_json::Value;
use sqlx::Row;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

/// Reads Canonical Thread History without giving migration code write access to its representation.
pub trait CanonicalThreadHistoryReader {
    /// Returns parsed records in source order and the number of rejected source records.
    fn read<'a>(
        &'a self,
        path: &'a Path,
    ) -> impl std::future::Future<Output = anyhow::Result<(Vec<RolloutLine>, usize)>> + Send + 'a;

    /// Derives canonical thread metadata through the rollout subsystem's compatibility parser.
    fn extract_metadata<'a>(
        &'a self,
        path: &'a Path,
    ) -> impl std::future::Future<Output = anyhow::Result<ExtractionOutcome>> + Send + 'a {
        async move {
            anyhow::bail!(
                "metadata extraction is unavailable for rollout-only thread {}",
                path.display()
            )
        }
    }

    /// Returns the latest legacy session-index name for each requested thread.
    fn find_thread_names<'a>(
        &'a self,
        _source_home: &'a Path,
        _thread_ids: &'a HashSet<ThreadId>,
    ) -> impl std::future::Future<Output = anyhow::Result<HashMap<ThreadId, String>>> + Send + 'a
    {
        async { Ok(HashMap::new()) }
    }
}

/// Read-only, backend-neutral snapshot of every authoritative local thread record.
#[derive(Debug, PartialEq)]
pub struct RuntimeStateThreadSnapshot {
    pub(super) threads: Vec<ThreadMigrationSnapshot>,
    pub(super) spawn_edges: Vec<ThreadSpawnEdgeSnapshot>,
    pub(super) backfill: BackfillCoordinationSnapshot,
}

impl RuntimeStateThreadSnapshot {
    pub fn threads(&self) -> &[ThreadMigrationSnapshot] {
        &self.threads
    }

    pub fn spawn_edges(&self) -> &[ThreadSpawnEdgeSnapshot] {
        &self.spawn_edges
    }

    pub fn backfill(&self) -> &BackfillCoordinationSnapshot {
        &self.backfill
    }
}

/// One thread's metadata, Canonical Thread History, and query projections.
#[derive(Debug, PartialEq)]
pub struct ThreadMigrationSnapshot {
    pub(super) metadata: ThreadMetadata,
    pub(super) memory_mode: ThreadMemoryMode,
    pub(super) canonical_history: CanonicalThreadHistorySnapshot,
    pub(super) dynamic_tools: Vec<DynamicToolSpec>,
    pub(super) projection_state: Option<ThreadHistoryProjectionStateSnapshot>,
    pub(super) turns: Vec<ThreadTurnProjectionSnapshot>,
    pub(super) items: Vec<ThreadItemProjectionSnapshot>,
}

impl ThreadMigrationSnapshot {
    pub fn metadata(&self) -> &ThreadMetadata {
        &self.metadata
    }

    pub fn memory_mode(&self) -> ThreadMemoryMode {
        self.memory_mode
    }

    pub fn canonical_history(&self) -> &CanonicalThreadHistorySnapshot {
        &self.canonical_history
    }

    pub fn dynamic_tools(&self) -> &[DynamicToolSpec] {
        &self.dynamic_tools
    }
}

/// Full-fidelity parsed rollout records plus compatibility-parser rejection accounting.
pub struct CanonicalThreadHistorySnapshot {
    lines: Vec<RolloutLine>,
    rejected_line_count: usize,
}

impl CanonicalThreadHistorySnapshot {
    pub(crate) fn new(lines: Vec<RolloutLine>, rejected_line_count: usize) -> Self {
        Self {
            lines,
            rejected_line_count,
        }
    }

    pub fn lines(&self) -> &[RolloutLine] {
        &self.lines
    }

    pub fn rejected_line_count(&self) -> usize {
        self.rejected_line_count
    }
}

impl std::fmt::Debug for CanonicalThreadHistorySnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CanonicalThreadHistorySnapshot")
            .field("lines", &serde_json::to_value(&self.lines))
            .field("rejected_line_count", &self.rejected_line_count)
            .finish()
    }
}

impl PartialEq for CanonicalThreadHistorySnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.rejected_line_count == other.rejected_line_count
            && serde_json::to_value(&self.lines).ok() == serde_json::to_value(&other.lines).ok()
    }
}

/// Persisted cursor for incrementally materialized local thread projections.
#[derive(Debug, PartialEq, Eq)]
pub struct ThreadHistoryProjectionStateSnapshot {
    pub(super) next_rollout_byte_offset: u64,
    pub(super) next_rollout_ordinal: u64,
}

/// Persisted local turn projection in canonical rollout order.
#[derive(Debug, PartialEq, Eq)]
pub struct ThreadTurnProjectionSnapshot {
    pub(super) turn_id: String,
    pub(super) rollout_ordinal: u64,
    pub(super) status: String,
    pub(super) error: Option<Value>,
    pub(super) started_at: Option<i64>,
    pub(super) completed_at: Option<i64>,
    pub(super) duration_ms: Option<i64>,
    pub(super) first_user_item_id: Option<String>,
    pub(super) final_agent_item_id: Option<String>,
}

/// Persisted local app-server item projection in canonical rollout order.
#[derive(Debug, PartialEq, Eq)]
pub struct ThreadItemProjectionSnapshot {
    pub(super) turn_id: String,
    pub(super) item_id: String,
    pub(super) rollout_ordinal: u64,
    pub(super) created_at_ms: i64,
    pub(super) item: Value,
    pub(super) item_type: String,
}

/// Directional persisted spawn-graph edge and lifecycle status.
#[derive(Debug, PartialEq, Eq)]
pub struct ThreadSpawnEdgeSnapshot {
    pub(super) parent_thread_id: ThreadId,
    pub(super) child_thread_id: ThreadId,
    pub(super) status: DirectionalThreadSpawnEdgeStatus,
}

/// Complete backfill lifecycle and lease-fencing state at migration time.
#[derive(Debug, PartialEq, Eq)]
pub struct BackfillCoordinationSnapshot {
    pub(super) state: BackfillState,
    pub(super) updated_at: DateTime<Utc>,
    pub(super) owner_id: Option<String>,
    pub(super) fencing_token: i64,
    pub(super) lease_expires_at: Option<DateTime<Utc>>,
}

/// Snapshot a source already accepted by [`super::preflight_runtime_state_migration`].
///
/// SQLite databases are opened immutable and rollout paths are resolved only from the preflight
/// inventory. The operation does not mutate source files, configuration, or the destination.
pub async fn snapshot_runtime_state_migration_threads(
    source: &SqliteConfig,
    inventory: &RuntimeStateMigrationInventory,
    history_reader: &impl CanonicalThreadHistoryReader,
) -> anyhow::Result<RuntimeStateThreadSnapshot> {
    let state_pool = source
        .open_immutable_pool(&state_db_path(source.home()))
        .await
        .context("open immutable migration state DB")?;
    let history_pool = source
        .open_immutable_pool(&thread_history_db_path(source.home()))
        .await
        .context("open immutable migration thread history DB")?;
    let result = snapshot(
        source,
        inventory,
        history_reader,
        &state_pool,
        &history_pool,
    )
    .await;
    state_pool.close().await;
    history_pool.close().await;
    result
}

async fn snapshot(
    source: &SqliteConfig,
    inventory: &RuntimeStateMigrationInventory,
    history_reader: &impl CanonicalThreadHistoryReader,
    state_pool: &sqlx::SqlitePool,
    history_pool: &sqlx::SqlitePool,
) -> anyhow::Result<RuntimeStateThreadSnapshot> {
    let rows = sqlx::query(
        "SELECT id, rollout_path, created_at_ms AS created_at, updated_at_ms AS updated_at, \
         recency_at_ms AS recency_at, source, history_mode, thread_source, agent_nickname, \
         agent_role, agent_path, model_provider, model, reasoning_effort, cwd, cli_version, title, \
         name, preview, sandbox_policy, approval_mode, tokens_used, first_user_message, archived_at, \
         git_sha, git_branch, git_origin_url, memory_mode FROM threads ORDER BY id",
    )
    .fetch_all(state_pool)
    .await?;
    let mut threads = Vec::with_capacity(rows.len());
    let rollout_files = inventory
        .rollout_files
        .iter()
        .filter_map(|file| {
            source_validation::logical_rollout_path(&file.relative_path)
                .map(|logical| (logical, file.relative_path.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut referenced_rollouts = HashSet::new();
    for row in rows {
        let memory_mode = serde_json::from_value(Value::String(row.try_get("memory_mode")?))
            .context("decode thread memory mode")?;
        let metadata = ThreadMetadata::try_from(ThreadRow::try_from_row(&row)?)?;
        let relative_path =
            source_validation::relative_rollout_path(source.home(), &metadata.rollout_path)?;
        let physical_path = rollout_files.get(&relative_path).with_context(|| {
            format!(
                "thread {} rollout was not present in the preflight inventory",
                metadata.id
            )
        })?;
        referenced_rollouts.insert(relative_path);
        let rollout_path = source.home().join(physical_path);
        let (lines, rejected_line_count) = history_reader
            .read(&rollout_path)
            .await
            .with_context(|| format!("read Canonical Thread History for {}", metadata.id))?;
        anyhow::ensure!(
            rejected_line_count == 0,
            "Canonical Thread History for {} contains {rejected_line_count} unsupported or malformed record(s); upgrade Codex or repair the rollout before migrating",
            metadata.id
        );
        let session_meta_id = session_meta_id(&lines);
        anyhow::ensure!(
            session_meta_id == Some(metadata.id),
            "thread {} metadata does not match the first SessionMeta in its rollout",
            metadata.id
        );
        threads.push(
            read_thread_snapshot(
                metadata,
                memory_mode,
                CanonicalThreadHistorySnapshot::new(lines, rejected_line_count),
                state_pool,
                history_pool,
            )
            .await?,
        );
    }
    for (logical_path, physical_path) in rollout_files {
        if referenced_rollouts.contains(&logical_path) {
            continue;
        }
        let rollout_path = source.home().join(&physical_path);
        let (lines, rejected_line_count) = history_reader.read(&rollout_path).await?;
        anyhow::ensure!(
            rejected_line_count == 0,
            "Canonical Thread History at {} contains {rejected_line_count} unsupported or malformed record(s); upgrade Codex or repair the rollout before migrating",
            rollout_path.display()
        );
        let thread_id = session_meta_id(&lines).with_context(|| {
            format!(
                "rollout-only thread {} has no SessionMeta",
                rollout_path.display()
            )
        })?;
        let mut outcome = history_reader.extract_metadata(&rollout_path).await?;
        anyhow::ensure!(
            outcome.metadata.id == thread_id,
            "rollout-only thread metadata does not match SessionMeta for {thread_id}"
        );
        if logical_path.starts_with("archived_sessions") && outcome.metadata.archived_at.is_none() {
            outcome.metadata.archived_at = Some(outcome.metadata.updated_at);
        }
        let memory_mode = outcome
            .memory_mode
            .map(|mode| serde_json::from_value(Value::String(mode)))
            .transpose()?
            .unwrap_or(ThreadMemoryMode::Enabled);
        threads.push(
            read_thread_snapshot(
                outcome.metadata,
                memory_mode,
                CanonicalThreadHistorySnapshot::new(lines, rejected_line_count),
                state_pool,
                history_pool,
            )
            .await?,
        );
    }
    let thread_ids = threads
        .iter()
        .map(|thread| thread.metadata.id)
        .collect::<HashSet<_>>();
    let names = history_reader
        .find_thread_names(source.home(), &thread_ids)
        .await?;
    for thread in &mut threads {
        if thread.metadata.history_mode == codex_protocol::protocol::ThreadHistoryMode::Legacy
            && let Some(name) = names.get(&thread.metadata.id)
        {
            thread.metadata.name = Some(name.clone());
        }
    }
    threads.sort_by_key(|thread| thread.metadata.id.to_string());
    Ok(RuntimeStateThreadSnapshot {
        threads,
        spawn_edges: read_spawn_edges(state_pool).await?,
        backfill: read_backfill(state_pool).await?,
    })
}

fn session_meta_id(lines: &[RolloutLine]) -> Option<ThreadId> {
    lines.iter().find_map(|line| match &line.item {
        RolloutItem::SessionMeta(meta) => Some(meta.meta.id),
        RolloutItem::ResponseItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::Compacted(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::EventMsg(_) => None,
    })
}

async fn read_thread_snapshot(
    metadata: ThreadMetadata,
    memory_mode: ThreadMemoryMode,
    canonical_history: CanonicalThreadHistorySnapshot,
    state_pool: &sqlx::SqlitePool,
    history_pool: &sqlx::SqlitePool,
) -> anyhow::Result<ThreadMigrationSnapshot> {
    let thread_id = metadata.id.to_string();
    let tool_rows = sqlx::query("SELECT namespace, name, description, input_schema, defer_loading FROM thread_dynamic_tools WHERE thread_id = ? ORDER BY position")
        .bind(&thread_id)
        .fetch_all(state_pool)
        .await?;
    let tools = tool_rows
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get("namespace")?,
                DynamicToolFunctionSpec {
                    name: row.try_get("name")?,
                    description: row.try_get("description")?,
                    input_schema: serde_json::from_str(&row.try_get::<String, _>("input_schema")?)?,
                    defer_loading: row.try_get("defer_loading")?,
                },
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let projection_row = sqlx::query("SELECT next_rollout_byte_offset, next_rollout_ordinal FROM thread_history_projection_state WHERE thread_id = ?")
        .bind(&thread_id)
        .fetch_optional(history_pool)
        .await?;
    let projection_state = projection_row
        .map(|row| {
            anyhow::Ok(ThreadHistoryProjectionStateSnapshot {
                next_rollout_byte_offset: nonnegative(row.try_get("next_rollout_byte_offset")?)?,
                next_rollout_ordinal: nonnegative(row.try_get("next_rollout_ordinal")?)?,
            })
        })
        .transpose()?;
    let turn_rows = sqlx::query("SELECT turn_id, rollout_ordinal, status, error_json, started_at, completed_at, duration_ms, first_user_item_id, final_agent_item_id FROM thread_turns WHERE thread_id = ? ORDER BY rollout_ordinal, turn_id")
        .bind(&thread_id)
        .fetch_all(history_pool)
        .await?;
    let turns = turn_rows
        .into_iter()
        .map(|row| {
            let error = row
                .try_get::<Option<String>, _>("error_json")?
                .map(|value| serde_json::from_str(&value))
                .transpose()?;
            Ok(ThreadTurnProjectionSnapshot {
                turn_id: row.try_get("turn_id")?,
                rollout_ordinal: nonnegative(row.try_get("rollout_ordinal")?)?,
                status: row.try_get("status")?,
                error,
                started_at: row.try_get("started_at")?,
                completed_at: row.try_get("completed_at")?,
                duration_ms: row.try_get("duration_ms")?,
                first_user_item_id: row.try_get("first_user_item_id")?,
                final_agent_item_id: row.try_get("final_agent_item_id")?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let item_rows = sqlx::query("SELECT turn_id, item_id, rollout_ordinal, created_at_ms, item_json, item_type FROM thread_items WHERE thread_id = ? ORDER BY rollout_ordinal, turn_id, item_id")
        .bind(&thread_id)
        .fetch_all(history_pool)
        .await?;
    let items = item_rows
        .into_iter()
        .map(|row| {
            Ok(ThreadItemProjectionSnapshot {
                turn_id: row.try_get("turn_id")?,
                item_id: row.try_get("item_id")?,
                rollout_ordinal: nonnegative(row.try_get("rollout_ordinal")?)?,
                created_at_ms: row.try_get("created_at_ms")?,
                item: serde_json::from_str(&row.try_get::<String, _>("item_json")?)?,
                item_type: row.try_get("item_type")?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let dynamic_tools = if tools.is_empty() {
        canonical_history
            .lines
            .iter()
            .rev()
            .find_map(|line| match &line.item {
                RolloutItem::SessionMeta(meta) => meta.meta.dynamic_tools.clone(),
                RolloutItem::ResponseItem(_)
                | RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. }
                | RolloutItem::Compacted(_)
                | RolloutItem::TurnContext(_)
                | RolloutItem::WorldState(_)
                | RolloutItem::EventMsg(_) => None,
            })
            .unwrap_or_default()
    } else {
        group_dynamic_tools_by_namespace(tools)
    };
    Ok(ThreadMigrationSnapshot {
        metadata,
        memory_mode,
        canonical_history,
        dynamic_tools,
        projection_state,
        turns,
        items,
    })
}

async fn read_spawn_edges(pool: &sqlx::SqlitePool) -> anyhow::Result<Vec<ThreadSpawnEdgeSnapshot>> {
    sqlx::query("SELECT parent_thread_id, child_thread_id, status FROM thread_spawn_edges ORDER BY parent_thread_id, child_thread_id")
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(ThreadSpawnEdgeSnapshot {
                parent_thread_id: ThreadId::try_from(row.try_get::<String, _>("parent_thread_id")?)?,
                child_thread_id: ThreadId::try_from(row.try_get::<String, _>("child_thread_id")?)?,
                status: row.try_get::<String, _>("status")?.parse().map_err(anyhow::Error::msg)?,
            })
        })
        .collect()
}

async fn read_backfill(pool: &sqlx::SqlitePool) -> anyhow::Result<BackfillCoordinationSnapshot> {
    let row = sqlx::query("SELECT status, last_watermark, last_success_at, updated_at, owner_id, fencing_token, lease_expires_at_ms FROM backfill_state WHERE id = 1")
        .fetch_one(pool)
        .await?;
    let status = BackfillStatus::parse(&row.try_get::<String, _>("status")?)?;
    let last_success_at = row
        .try_get::<Option<i64>, _>("last_success_at")?
        .map(timestamp_seconds)
        .transpose()?;
    let lease_expires_at = row
        .try_get::<Option<i64>, _>("lease_expires_at_ms")?
        .map(timestamp_millis)
        .transpose()?;
    Ok(BackfillCoordinationSnapshot {
        state: BackfillState {
            status,
            last_watermark: row.try_get("last_watermark")?,
            last_success_at,
        },
        updated_at: timestamp_seconds(row.try_get("updated_at")?)?,
        owner_id: row.try_get("owner_id")?,
        fencing_token: row.try_get("fencing_token")?,
        lease_expires_at,
    })
}

fn nonnegative(value: i64) -> anyhow::Result<u64> {
    u64::try_from(value).context("negative thread history projection position")
}

fn timestamp_seconds(value: i64) -> anyhow::Result<DateTime<Utc>> {
    DateTime::from_timestamp(value, 0).context("invalid backfill timestamp")
}

fn timestamp_millis(value: i64) -> anyhow::Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(value).context("invalid backfill lease timestamp")
}
