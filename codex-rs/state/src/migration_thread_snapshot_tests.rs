use super::CanonicalThreadHistoryReader;
use super::RuntimeStateMigrationInventory;
use super::RuntimeStateThreadSnapshot;
use super::SourceFileInventory;
use super::snapshot_runtime_state_migration_threads;
use super::test_support;
use crate::BackfillCoordinationSnapshot;
use crate::DirectionalThreadSpawnEdgeStatus;
use crate::SqliteConfig;
use crate::ThreadHistoryProjectionStateSnapshot;
use crate::ThreadItemProjectionSnapshot;
use crate::ThreadMigrationSnapshot;
use crate::ThreadSpawnEdgeSnapshot;
use crate::ThreadTurnProjectionSnapshot;
use crate::open_thread_history_db;
use crate::runtime::test_support::test_thread_metadata;
use codex_history::RolloutItem;
use codex_history::RolloutLine;
use codex_protocol::ThreadId;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

struct FixtureHistoryReader {
    histories: HashMap<PathBuf, Vec<RolloutLine>>,
    minimum_read_budget: u64,
}

impl CanonicalThreadHistoryReader for FixtureHistoryReader {
    async fn read(
        &self,
        path: &Path,
        maximum_source_bytes: u64,
    ) -> anyhow::Result<(Vec<RolloutLine>, usize, u64)> {
        anyhow::ensure!(
            maximum_source_bytes >= self.minimum_read_budget,
            "fixture received a shared aggregate read budget"
        );
        let lines = self
            .histories
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing fixture history: {}", path.display()))?;
        let source_bytes = serde_json::to_vec(&lines)?.len().try_into()?;
        anyhow::ensure!(
            source_bytes <= maximum_source_bytes,
            "fixture exceeds read budget"
        );
        Ok((lines, 0, source_bytes))
    }
}

#[tokio::test]
async fn snapshot_accepts_missing_thread_history_database() -> anyhow::Result<()> {
    let (source, runtime) = test_support::initialized_runtime_source("no-thread-history").await?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    runtime.close().await;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?);
    std::fs::remove_file(sqlite.thread_history_db_path())?;
    let source_before = test_support::snapshot_source(&source)?;
    let reader = FixtureHistoryReader {
        histories: HashMap::new(),
        minimum_read_budget: 0,
    };

    let snapshot =
        snapshot_runtime_state_migration_threads(&sqlite, &inventory(&source, [])?, &reader)
            .await?;

    assert!(snapshot.threads().is_empty());
    assert_eq!(test_support::snapshot_source(&source)?, source_before);
    Ok(())
}

#[tokio::test]
async fn snapshot_preserves_complete_legacy_and_current_thread_domain_read_only()
-> anyhow::Result<()> {
    let (source, runtime) = test_support::initialized_runtime_source("thread-snapshot").await?;
    let cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let legacy_id = ThreadId::from_string("019c84d0-1111-7777-8111-111111111111")?;
    let current_id = ThreadId::from_string("019c84d0-2222-7777-8222-222222222222")?;
    let legacy_path = source.join("sessions/2026/07/21/rollout-legacy.jsonl");
    let current_path = source.join("archived_sessions/rollout-current.jsonl");
    std::fs::create_dir_all(legacy_path.parent().expect("legacy parent"))?;
    std::fs::create_dir_all(current_path.parent().expect("current parent"))?;
    std::fs::write(&legacy_path, b"legacy rollout\n")?;
    std::fs::write(&current_path, b"current rollout\n")?;

    let mut legacy = test_thread_metadata(&source, legacy_id, source.join("legacy-cwd"));
    legacy.rollout_path = legacy_path.clone();
    legacy.title = "Legacy name".to_string();
    legacy.git_sha = Some("legacy-sha".to_string());
    legacy.git_branch = Some("legacy-branch".to_string());
    legacy.git_origin_url = Some("https://example.test/legacy.git".to_string());
    runtime.upsert_thread(&legacy).await?;
    runtime
        .set_thread_memory_mode(legacy_id, "polluted")
        .await?;

    let mut current = test_thread_metadata(&source, current_id, source.join("current-cwd"));
    current.rollout_path = current_path.clone();
    current.history_mode = ThreadHistoryMode::Paginated;
    current.name = Some("Current name".to_string());
    current.archived_at = chrono::DateTime::from_timestamp(1_753_200_000, 0);
    runtime.upsert_thread(&current).await?;
    current.updated_at += chrono::Duration::milliseconds(1);
    current.recency_at += chrono::Duration::milliseconds(1);
    runtime
        .upsert_thread_spawn_edge(
            legacy_id,
            current_id,
            DirectionalThreadSpawnEdgeStatus::Closed,
        )
        .await?;
    runtime.close().await;

    seed_sqlite_thread_state(&source, current_id).await?;
    seed_thread_history_projections(&source, current_id).await?;
    let source_before = test_support::snapshot_source(&source)?;

    let mut inherited_meta = session_meta(legacy_id, &legacy, /*parent_thread_id*/ None);
    inherited_meta.meta.dynamic_tools = Some(dynamic_tools_fixture());
    let legacy_history = vec![
        rollout_line(
            "2026-07-21T10:00:00Z",
            /*ordinal*/ None,
            RolloutItem::SessionMeta(session_meta(
                legacy_id, &legacy, /*parent_thread_id*/ None,
            )),
        ),
        rollout_line(
            "2026-07-21T10:01:00Z",
            /*ordinal*/ None,
            RolloutItem::SessionMeta(inherited_meta),
        ),
    ];
    let mut current_meta = session_meta(current_id, &current, Some(legacy_id));
    current_meta.meta.history_mode = ThreadHistoryMode::Paginated;
    current_meta.meta.history_base = Some(HistoryPosition {
        thread_id: legacy_id,
        end_ordinal_exclusive: 3,
        end_byte_offset: 144,
    });
    current_meta.meta.dynamic_tools = Some(dynamic_tools_fixture());
    let current_history = vec![
        rollout_line(
            "2026-07-22T10:00:00.123Z",
            Some(0),
            RolloutItem::SessionMeta(current_meta),
        ),
        rollout_line(
            "2026-07-22T10:01:00.456Z",
            Some(1),
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                num_turns: 1,
            })),
        ),
    ];
    let reader = FixtureHistoryReader {
        histories: HashMap::from([
            (legacy_path.clone(), legacy_history.clone()),
            (current_path.clone(), current_history.clone()),
        ]),
        minimum_read_budget: 256 * 1024 * 1024,
    };
    let inventory = inventory(&source, [&legacy_path, &current_path])?;

    let actual = snapshot_runtime_state_migration_threads(
        &SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?),
        &inventory,
        &reader,
    )
    .await?;
    let expected = RuntimeStateThreadSnapshot {
        threads: vec![
            ThreadMigrationSnapshot {
                metadata: legacy,
                memory_mode: ThreadMemoryMode::Enabled,
                polluted_at_stream_version: Some(2),
                canonical_history: super::CanonicalThreadHistorySnapshot::new(
                    legacy_history,
                    /*rejected_line_count*/ 0,
                ),
                dynamic_tools: Vec::new(),
                projection_state: None,
                turns: Vec::new(),
                items: Vec::new(),
            },
            ThreadMigrationSnapshot {
                metadata: current,
                memory_mode: ThreadMemoryMode::Enabled,
                polluted_at_stream_version: None,
                canonical_history: super::CanonicalThreadHistorySnapshot::new(
                    current_history,
                    /*rejected_line_count*/ 0,
                ),
                dynamic_tools: dynamic_tools_fixture(),
                projection_state: Some(ThreadHistoryProjectionStateSnapshot {
                    next_rollout_byte_offset: 321,
                    next_rollout_ordinal: 2,
                }),
                turns: vec![ThreadTurnProjectionSnapshot {
                    turn_id: "turn-1".to_string(),
                    rollout_ordinal: 1,
                    status: "completed".to_string(),
                    error: None,
                    started_at: Some(10),
                    completed_at: Some(20),
                    duration_ms: Some(10_000),
                    first_user_item_id: Some("user-1".to_string()),
                    final_agent_item_id: Some("agent-1".to_string()),
                }],
                items: vec![ThreadItemProjectionSnapshot {
                    turn_id: "turn-1".to_string(),
                    item_id: "user-1".to_string(),
                    rollout_ordinal: 1,
                    updated_at_ordinal: 0,
                    created_at_ms: 12_345,
                    item: json!({"id":"user-1","type":"userMessage"}),
                    item_type: "userMessage".to_string(),
                }],
            },
        ],
        spawn_edges: vec![ThreadSpawnEdgeSnapshot {
            parent_thread_id: legacy_id,
            child_thread_id: current_id,
            status: DirectionalThreadSpawnEdgeStatus::Closed,
        }],
        backfill: BackfillCoordinationSnapshot {
            state: crate::BackfillState {
                status: crate::BackfillStatus::Complete,
                last_watermark: Some("sessions/complete".to_string()),
                last_success_at: chrono::DateTime::from_timestamp(1_753_200_100, 0),
            },
            updated_at: chrono::DateTime::from_timestamp(1_753_200_200, 0)
                .expect("updated timestamp"),
            owner_id: None,
            fencing_token: 7,
            lease_expires_at: None,
        },
    };

    assert_eq!(actual, expected);
    assert_eq!(test_support::snapshot_source(&source)?, source_before);
    drop(cleanup);
    Ok(())
}

async fn seed_sqlite_thread_state(source: &Path, thread_id: ThreadId) -> anyhow::Result<()> {
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source)?);
    let pool = sqlite.open_read_write_pool(&sqlite.state_db_path()).await?;
    sqlx::query("INSERT INTO thread_dynamic_tools (thread_id, position, name, description, input_schema, defer_loading, namespace) VALUES (?, 0, 'lookup', 'Lookup ticket', '{\"type\":\"object\"}', 1, 'tickets')")
        .bind(thread_id.to_string())
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE backfill_state SET status = 'complete', last_watermark = 'sessions/complete', last_success_at = 1753200100, updated_at = 1753200200, owner_id = NULL, fencing_token = 7, lease_expires_at_ms = NULL WHERE id = 1")
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}

async fn seed_thread_history_projections(source: &Path, thread_id: ThreadId) -> anyhow::Result<()> {
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source)?);
    let pool = open_thread_history_db(&sqlite).await?;
    sqlx::query("INSERT INTO thread_history_projection_state (thread_id, next_rollout_byte_offset, next_rollout_ordinal) VALUES (?, 321, 2)")
        .bind(thread_id.to_string())
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO thread_turns (thread_id, turn_id, rollout_ordinal, status, started_at, completed_at, duration_ms, first_user_item_id, final_agent_item_id) VALUES (?, 'turn-1', 1, 'completed', 10, 20, 10000, 'user-1', 'agent-1')")
        .bind(thread_id.to_string())
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json, item_type) VALUES (?, 'turn-1', 'user-1', 1, 12345, '{\"id\":\"user-1\",\"type\":\"userMessage\"}', 'userMessage')")
        .bind(thread_id.to_string())
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}

fn inventory<const N: usize>(
    source: &Path,
    paths: [&Path; N],
) -> anyhow::Result<RuntimeStateMigrationInventory> {
    Ok(RuntimeStateMigrationInventory {
        databases: Vec::new(),
        rollout_files: paths
            .into_iter()
            .map(|path| {
                Ok(SourceFileInventory {
                    relative_path: path.strip_prefix(source)?.to_path_buf(),
                    size_bytes: std::fs::metadata(path)?.len(),
                    sha256: String::new(),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        memory_files: Vec::new(),
        imported_resources: Vec::new(),
        configuration: None,
        session_index: None,
        destination_schema: "unused".to_string(),
        destination_schema_version: 20,
    })
}

fn rollout_line(timestamp: &str, ordinal: Option<u64>, item: RolloutItem) -> RolloutLine {
    RolloutLine {
        timestamp: timestamp.to_string(),
        ordinal,
        item,
    }
}

fn session_meta(
    thread_id: ThreadId,
    metadata: &crate::ThreadMetadata,
    parent_thread_id: Option<ThreadId>,
) -> SessionMetaLine {
    SessionMetaLine {
        meta: SessionMeta {
            session_id: thread_id.into(),
            id: thread_id,
            forked_from_id: parent_thread_id,
            parent_thread_id,
            timestamp: metadata.created_at.to_rfc3339(),
            cwd: metadata.cwd.clone(),
            originator: "migration-fixture".to_string(),
            cli_version: metadata.cli_version.clone(),
            source: serde_json::from_value(serde_json::Value::String(metadata.source.clone()))
                .expect("session source"),
            model_provider: Some(metadata.model_provider.clone()),
            memory_mode: Some("enabled".to_string()),
            ..SessionMeta::default()
        },
        git: None,
    }
}

fn dynamic_tools_fixture() -> Vec<DynamicToolSpec> {
    vec![DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
        name: "tickets".to_string(),
        description: String::new(),
        tools: vec![DynamicToolNamespaceTool::Function(
            DynamicToolFunctionSpec {
                name: "lookup".to_string(),
                description: "Lookup ticket".to_string(),
                input_schema: json!({"type":"object"}),
                defer_loading: true,
            },
        )],
    })]
}
