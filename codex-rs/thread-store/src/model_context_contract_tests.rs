use std::path::Path;
use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::TruncationPolicy;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::CreateThreadParams;
use crate::LoadThreadHistoryParams;
use crate::LocalThreadStore;
use crate::LocalThreadStoreConfig;
use crate::PostgresThreadStore;
use crate::ResumeThreadParams;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::postgres_contract_tests::PostgresThreadStoreFixture;

#[tokio::test]
async fn local_latest_model_context_matches_public_store_contract()
-> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let store = LocalThreadStore::new(
        LocalThreadStoreConfig {
            codex_home: home.path().to_path_buf(),
            sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
            default_model_provider_id: "model-context-contract".to_string(),
        },
        /*state_db*/ None,
    );
    let thread_ids = [
        ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f031")?,
        ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f033")?,
        ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f034")?,
    ];

    assert_latest_model_context_contract(&store, thread_ids, home.path()).await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_latest_model_context_matches_public_store_contract()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("model_context_shared")?;
    fixture.migrate().await?;
    let pool = fixture.connect_pool().await?;
    let store = PostgresThreadStore::new(pool.clone(), fixture.schema.clone());
    let thread_ids = [
        ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f032")?,
        ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f035")?,
        ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f036")?,
    ];

    assert_latest_model_context_contract(&store, thread_ids, Path::new("/model-context-contract"))
        .await?;

    pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_model_context_rejects_decoded_item_over_token_cap()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("model_context_item_cap")?;
    fixture.migrate().await?;
    let pool = fixture.connect_pool().await?;
    let store = PostgresThreadStore::new(pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f038")?;
    let item_byte_limit = TruncationPolicy::Tokens(/*tokens*/ 10_000).byte_budget();
    store
        .create_thread(create_thread_params(
            thread_id,
            Path::new("/model-context-item-cap"),
            ThreadHistoryMode::Legacy,
        ))
        .await?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "x".repeat(item_byte_limit + 1),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            })],
        })
        .await?;
    store.shutdown_thread(thread_id).await?;

    let stored_bytes: i32 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT pg_column_size(item) FROM {} WHERE thread_id = $1 AND ordinal = 1",
        store.tables.history
    )))
    .bind(thread_id.to_string())
    .fetch_one(&pool)
    .await?;
    assert!(
        usize::try_from(stored_bytes)? < item_byte_limit,
        "fixture must exercise a compressed JSONB value"
    );
    let error = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await
        .expect_err("decoded item above the token cap must be rejected");
    assert_eq!(
        error.to_string(),
        format!(
            "invalid thread-store request: model context for thread {thread_id} cannot be loaded safely: an individual history item exceeds 10000 estimated tokens (limit: 10000 items or 16777216 bytes)"
        )
    );

    pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_resume_and_context_are_database_native_across_replicas()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("model_context_replica")?;
    fixture.migrate().await?;
    let writer_pool = fixture.connect_pool().await?;
    let replica_pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(writer_pool.clone(), fixture.schema.clone());
    let replica = PostgresThreadStore::new(replica_pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f037")?;
    let cwd = Path::new("/database-native-context");
    writer
        .create_thread(create_thread_params(
            thread_id,
            cwd,
            ThreadHistoryMode::Paginated,
        ))
        .await?;
    let mut expected_items = bounded_model_context_history(thread_id, cwd);
    writer
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: expected_items.clone(),
        })
        .await?;
    writer.shutdown_thread(thread_id).await?;

    replica
        .resume_thread(ResumeThreadParams {
            thread_id,
            rollout_path: Some(PathBuf::from("rollout-that-must-not-be-read.jsonl")),
            history: None,
            include_archived: false,
            metadata: ThreadPersistenceMetadata {
                cwd: Some(cwd.to_path_buf()),
                model_provider: "model-context-contract".to_string(),
                memory_mode: ThreadMemoryMode::Enabled,
            },
        })
        .await?;
    let resumed_history = replica
        .load_history(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    assert_eq!(
        serde_json::to_value(&resumed_history.items[1..])?,
        serde_json::to_value(&expected_items)?
    );

    let latest_turn = model_context_turn(thread_id, cwd, "replica", /*window_number*/ 3);
    replica
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: latest_turn.clone(),
        })
        .await?;
    replica.shutdown_thread(thread_id).await?;
    expected_items.extend(latest_turn.clone());

    let final_history = writer
        .load_history(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    assert_eq!(
        serde_json::to_value(&final_history.items[1..])?,
        serde_json::to_value(&expected_items)?
    );
    let context = writer
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    assert_eq!(context.items.len(), latest_turn.len() + 1);
    let Some(RolloutItem::SessionMeta(meta)) = context.items.first() else {
        panic!("model context should start with canonical session metadata");
    };
    assert_eq!(meta.meta.id, thread_id);
    assert_eq!(meta.meta.dynamic_tools, Some(dynamic_tools_fixture()));
    assert_eq!(
        meta.meta.selected_capability_roots,
        selected_capability_roots_fixture()
    );
    assert_eq!(
        serde_json::to_value(&context.items[1..])?,
        serde_json::to_value(latest_turn)?
    );

    writer_pool.close().await;
    replica_pool.close().await;
    fixture.cleanup().await
}

async fn assert_latest_model_context_contract(
    store: &dyn ThreadStore,
    thread_ids: [ThreadId; 3],
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let [thread_id, legacy_thread_id, unbounded_thread_id] = thread_ids;
    store
        .create_thread(create_thread_params(
            thread_id,
            cwd,
            ThreadHistoryMode::Paginated,
        ))
        .await?;
    let history = bounded_model_context_history(thread_id, cwd);
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: history.clone(),
        })
        .await?;
    store.flush_thread(thread_id).await?;

    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;

    assert_eq!(context.thread_id, thread_id);
    assert_eq!(context.items.len(), history[6..].len() + 1);
    let Some(RolloutItem::SessionMeta(meta)) = context.items.first() else {
        panic!("model context should start with canonical session metadata");
    };
    assert_eq!(meta.meta.id, thread_id);
    assert_eq!(meta.meta.dynamic_tools, Some(dynamic_tools_fixture()));
    assert_eq!(
        meta.meta.selected_capability_roots,
        selected_capability_roots_fixture()
    );
    assert_eq!(
        serde_json::to_value(&context.items[1..])?,
        serde_json::to_value(&history[6..])?
    );

    store.shutdown_thread(thread_id).await?;
    store
        .archive_thread(ArchiveThreadParams { thread_id })
        .await?;
    assert!(matches!(
        store
            .load_latest_model_context(LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await,
        Err(ThreadStoreError::InvalidRequest { .. })
    ));
    let archived_context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: true,
        })
        .await?;
    assert_eq!(
        serde_json::to_value(archived_context.items)?,
        serde_json::to_value(context.items)?
    );

    store
        .create_thread(create_thread_params(
            legacy_thread_id,
            cwd,
            ThreadHistoryMode::Legacy,
        ))
        .await?;
    let legacy_history = bounded_model_context_history(legacy_thread_id, cwd);
    store
        .append_items(AppendThreadItemsParams {
            thread_id: legacy_thread_id,
            items: legacy_history,
        })
        .await?;
    assert_full_model_context(store, legacy_thread_id).await?;

    store
        .create_thread(create_thread_params(
            unbounded_thread_id,
            cwd,
            ThreadHistoryMode::Paginated,
        ))
        .await?;
    let mut unbounded_history = bounded_model_context_history(unbounded_thread_id, cwd);
    let latest_compaction = unbounded_history
        .iter_mut()
        .rev()
        .find_map(|item| match item {
            RolloutItem::Compacted(compacted) => Some(compacted),
            _ => None,
        })
        .ok_or("model context fixture must contain compaction")?;
    latest_compaction.replacement_history = None;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: unbounded_thread_id,
            items: unbounded_history.clone(),
        })
        .await?;
    let unbounded_context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: unbounded_thread_id,
            include_archived: false,
        })
        .await?;
    assert!(matches!(
        unbounded_context.items.first(),
        Some(RolloutItem::SessionMeta(meta)) if meta.meta.id == unbounded_thread_id
    ));
    assert_eq!(unbounded_context.items.len(), unbounded_history.len() + 1);
    assert_eq!(
        serde_json::to_value(&unbounded_context.items[1..])?,
        serde_json::to_value(unbounded_history)?
    );
    Ok(())
}

async fn assert_full_model_context(
    store: &dyn ThreadStore,
    thread_id: ThreadId,
) -> Result<(), Box<dyn std::error::Error>> {
    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    assert_eq!(
        serde_json::to_value(context.items)?,
        serde_json::to_value(history.items)?
    );
    Ok(())
}

fn bounded_model_context_history(thread_id: ThreadId, cwd: &Path) -> Vec<RolloutItem> {
    [
        model_context_turn(thread_id, cwd, "older", /*window_number*/ 1),
        model_context_turn(thread_id, cwd, "latest", /*window_number*/ 2),
    ]
    .concat()
}

fn model_context_turn(
    thread_id: ThreadId,
    cwd: &Path,
    turn_id: &str,
    window_number: u64,
) -> Vec<RolloutItem> {
    vec![
        turn_started(turn_id),
        user_response(turn_id),
        completed_user_message(thread_id, turn_id),
        turn_context(cwd, turn_id),
        RolloutItem::Compacted(CompactedItem {
            message: format!("{turn_id} checkpoint"),
            replacement_history: Some(Vec::new()),
            window_number: Some(window_number),
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: turn_id.to_string(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
    ]
}

fn turn_started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: Some(128_000),
        collaboration_mode_kind: Default::default(),
    }))
}

fn user_response(turn_id: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: format!("{turn_id} user message"),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

fn completed_user_message(thread_id: ThreadId, turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: turn_id.to_string(),
        item: TurnItem::UserMessage(UserMessageItem {
            id: format!("user-{turn_id}"),
            client_id: None,
            content: vec![UserInput::Text {
                text: format!("{turn_id} user message"),
                text_elements: Vec::new(),
            }],
        }),
        started_at_ms: Some(0),
        completed_at_ms: 0,
    }))
}

fn turn_context(cwd: &Path, turn_id: &str) -> RolloutItem {
    RolloutItem::TurnContext(TurnContextItem {
        turn_id: Some(turn_id.to_string()),
        cwd: serde_json::from_value(serde_json::json!(cwd)).expect("absolute contract cwd"),
        workspace_roots: None,
        current_date: None,
        timezone: None,
        approval_policy: AskForApproval::Never,
        approvals_reviewer: None,
        sandbox_policy: SandboxPolicy::new_read_only_policy(),
        permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: "contract-model".to_string(),
        comp_hash: None,
        personality: None,
        collaboration_mode: None,
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: None,
        effort: None,
        summary: ReasoningSummary::Auto,
    })
}

fn create_thread_params(
    thread_id: ThreadId,
    cwd: &Path,
    history_mode: ThreadHistoryMode,
) -> CreateThreadParams {
    CreateThreadParams {
        session_id: thread_id.into(),
        thread_id,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "model-context-contract".to_string(),
        base_instructions: BaseInstructions::default(),
        dynamic_tools: dynamic_tools_fixture(),
        selected_capability_roots: selected_capability_roots_fixture(),
        multi_agent_version: None,
        history_mode,
        history_base: None,
        subagent_history_start_ordinal: None,
        initial_window_id: "model-context-window".to_string(),
        metadata: ThreadPersistenceMetadata {
            cwd: Some(cwd.to_path_buf()),
            model_provider: "model-context-contract".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

fn dynamic_tools_fixture() -> Vec<DynamicToolSpec> {
    vec![
        dynamic_tool_namespace("alpha", "Alpha tools", "ticketId", "archive_ticket"),
        dynamic_tool_namespace("beta", "Beta tools", "repository", "archive_repository"),
    ]
}

fn dynamic_tool_namespace(
    name: &str,
    description: &str,
    required_property: &str,
    deferred_name: &str,
) -> DynamicToolSpec {
    DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
        name: name.to_string(),
        description: description.to_string(),
        tools: vec![
            DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec {
                name: "lookup".to_string(),
                description: format!("Look up by {required_property}"),
                input_schema: json!({
                    "type": "object",
                    "properties": { required_property: { "type": "string" } },
                    "required": [required_property],
                    "additionalProperties": false,
                }),
                defer_loading: false,
            }),
            DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec {
                name: deferred_name.to_string(),
                description: format!("Deferred {name} operation"),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                defer_loading: true,
            }),
        ],
    })
}

fn selected_capability_roots_fixture() -> Vec<SelectedCapabilityRoot> {
    vec![
        serde_json::from_value(json!({
            "id": "selected-dynamic-tools",
            "location": {
                "type": "environment",
                "environmentId": "executor-test",
                "path": "file:///plugins/dynamic-tools"
            }
        }))
        .expect("selected capability root fixture should deserialize"),
    ]
}
