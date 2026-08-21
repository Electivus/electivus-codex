#![allow(
    clippy::disallowed_methods,
    reason = "PostgreSQL tests connect only to PostgreSQL pools"
)]

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server::in_process::InProcessClientHandle;
use codex_app_server::in_process::InProcessServerEvent;
use codex_app_server_protocol as api;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::user_input::UserInput;
use codex_rollout::CompactedItem;
use codex_rollout::RolloutItem;
use codex_state::PostgresNamespaceAction;
use codex_state::PostgresNamespaceConfig;
use codex_state::PostgresPoolConfig;
use codex_state::RuntimeStateBackendConfig;
use codex_state::StateRuntime;
use codex_thread_store as store;
use codex_thread_store::PersistContext;
use codex_thread_store::ThreadStore;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use sqlx::AssertSqlSafe;
use tempfile::TempDir;
use tokio::time::timeout;
use uuid::Uuid;

use super::remote_thread_store::start_in_process_server_with_thread_store;

const DATABASE_URL_ENV: &str = "CODEX_TEST_POSTGRES_URL";
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_store_serves_database_native_v2_history_flows() -> Result<()> {
    let fixture = PostgresFixture::new()?;
    fixture.migrate().await?;
    let model_server =
        create_mock_responses_server_repeating_assistant("after cross-host resume answer").await;
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            r#"
model = "mock-model"
model_provider = "mock_provider"
approval_policy = "never"
sandbox_mode = "read-only"

[model_providers.mock_provider]
name = "Mock provider"
base_url = "{}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0

[features]
plugins = false
"#,
            model_server.uri()
        ),
    )?;
    let writer_runtime = fixture.runtime(codex_home.path()).await?;
    let reader_runtime = fixture.runtime(codex_home.path()).await?;
    let writer = store::PostgresThreadStore::from_runtime(Arc::clone(&writer_runtime))?;
    let thread_id = ThreadId::new();
    seed_thread(&writer, thread_id, codex_home.path(), "mock_provider").await?;
    let foreign_cwd = fixture
        .append_foreign_host_context(thread_id, codex_home.path(), &writer)
        .await?;

    let thread_store: Arc<dyn store::ThreadStore> = Arc::new(
        store::PostgresThreadStore::from_runtime(Arc::clone(&reader_runtime))?,
    );
    let mut client =
        start_in_process_server_with_thread_store(codex_home.path(), thread_store).await?;
    let thread_id = thread_id.to_string();

    let list: api::ThreadListResponse = request(
        &client,
        api::ClientRequest::ThreadList {
            request_id: request_id(/*id*/ 1),
            params: api::ThreadListParams {
                cursor: None,
                limit: Some(1),
                sort_key: Some(api::ThreadSortKey::CreatedAt),
                sort_direction: Some(api::SortDirection::Asc),
                model_providers: Some(Vec::new()),
                source_kinds: Some(vec![api::ThreadSourceKind::Exec]),
                archived: Some(false),
                is_pinned: None,
                section_id: None,
                cwd: None,
                use_state_db_only: true,
                search_term: None,
                parent_thread_id: None,
                ancestor_thread_id: None,
                project_cwd: None,
            },
        },
    )
    .await?;
    assert_eq!(
        list.data
            .iter()
            .map(|thread| &thread.id)
            .collect::<Vec<_>>(),
        vec![&thread_id]
    );
    assert_eq!(list.data[0].history_mode, api::ThreadHistoryMode::Paginated);
    assert_eq!(list.data[0].path, None);

    let read: api::ThreadReadResponse = request(
        &client,
        api::ClientRequest::ThreadRead {
            request_id: request_id(/*id*/ 2),
            params: api::ThreadReadParams {
                thread_id: thread_id.clone(),
                include_turns: false,
            },
        },
    )
    .await?;
    assert_eq!(read.thread.id, thread_id);
    assert_eq!(read.thread.preview, "database native needle");
    assert_eq!(read.thread.cwd.as_str(), foreign_cwd);

    let first_turn: api::ThreadTurnsListResponse = request(
        &client,
        api::ClientRequest::ThreadTurnsList {
            request_id: request_id(/*id*/ 3),
            params: api::ThreadTurnsListParams {
                thread_id: thread_id.clone(),
                cursor: None,
                limit: Some(1),
                sort_direction: Some(api::SortDirection::Asc),
                items_view: Some(api::TurnItemsView::Full),
            },
        },
    )
    .await?;
    assert_eq!(turn_ids(&first_turn), vec!["turn-1"]);
    let second_turn: api::ThreadTurnsListResponse = request(
        &client,
        api::ClientRequest::ThreadTurnsList {
            request_id: request_id(/*id*/ 4),
            params: api::ThreadTurnsListParams {
                thread_id: thread_id.clone(),
                cursor: first_turn.next_cursor,
                limit: Some(1),
                sort_direction: Some(api::SortDirection::Asc),
                items_view: Some(api::TurnItemsView::Full),
            },
        },
    )
    .await?;
    assert_eq!(turn_ids(&second_turn), vec!["turn-2"]);

    let first_items = items_page(&client, &thread_id, /*cursor*/ None).await?;
    assert_eq!(item_ids(&first_items), vec!["user-1", "agent-1"]);
    let second_items = items_page(&client, &thread_id, first_items.next_cursor).await?;
    assert_eq!(item_ids(&second_items), vec!["user-2", "agent-2"]);

    let search: api::ThreadSearchResponse = request(
        &client,
        api::ClientRequest::ThreadSearch {
            request_id: request_id(/*id*/ 7),
            params: api::ThreadSearchParams {
                cursor: None,
                limit: Some(1),
                sort_key: Some(api::ThreadSearchSortKey::CreatedAt),
                sort_direction: Some(api::SortDirection::Asc),
                source_kinds: Some(vec![api::ThreadSourceKind::Exec]),
                archived: Some(false),
                search_term: "database native".to_string(),
            },
        },
    )
    .await?;
    assert_eq!(search.data[0].thread.id, thread_id);
    assert!(search.data[0].snippet.contains("database native needle"));

    let first_occurrences = occurrence_page(&client, &thread_id, /*cursor*/ None).await?;
    assert_eq!(
        occurrence_ids(&first_occurrences),
        vec!["user-1", "agent-1"]
    );
    let second_occurrences =
        occurrence_page(&client, &thread_id, first_occurrences.next_cursor).await?;
    assert_eq!(
        occurrence_ids(&second_occurrences),
        vec!["user-2", "agent-2"]
    );

    let resumed: api::ThreadResumeResponse = request(
        &client,
        api::ClientRequest::ThreadResume {
            request_id: request_id(/*id*/ 10),
            params: api::ThreadResumeParams {
                thread_id: thread_id.clone(),
                path: Some(std::path::PathBuf::from(
                    "/legacy/path/that-is-not-authoritative.jsonl",
                )),
                exclude_turns: true,
                initial_turns_page: Some(api::ThreadResumeInitialTurnsPageParams {
                    limit: Some(1),
                    sort_direction: Some(api::SortDirection::Desc),
                    items_view: Some(api::TurnItemsView::Full),
                }),
                ..Default::default()
            },
        },
    )
    .await?;
    assert_eq!(resumed.thread.path, None);
    assert_eq!(resumed.thread.turns, Vec::new());
    assert_eq!(
        resumed
            .initial_turns_page
            .as_ref()
            .map(|page| turn_ids_from(&page.data)),
        Some(vec!["turn-2"])
    );
    assert!(resumed.turns_backwards_cursor.is_some());
    assert!(resumed.items_backwards_cursor.is_some());

    let error = client
        .request(api::ClientRequest::ThreadFork {
            request_id: request_id(/*id*/ 11),
            params: api::ThreadForkParams {
                thread_id: thread_id.clone(),
                ephemeral: true,
                ..Default::default()
            },
        })
        .await?
        .expect_err("ephemeral paginated forks should require metadata-only responses");
    assert_eq!(error.code, -32600);
    assert_eq!(
        error.message,
        "ephemeral paginated thread/fork requires `excludeTurns: true`"
    );

    let ephemeral_fork: api::ThreadForkResponse = request(
        &client,
        api::ClientRequest::ThreadFork {
            request_id: request_id(/*id*/ 12),
            params: api::ThreadForkParams {
                thread_id: thread_id.clone(),
                ephemeral: true,
                exclude_turns: true,
                ..Default::default()
            },
        },
    )
    .await?;
    assert!(ephemeral_fork.thread.ephemeral);
    assert_eq!(ephemeral_fork.thread.path, None);
    assert_eq!(ephemeral_fork.thread.turns, Vec::new());

    let forked: api::ThreadForkResponse = request(
        &client,
        api::ClientRequest::ThreadFork {
            request_id: request_id(/*id*/ 13),
            params: api::ThreadForkParams {
                thread_id: thread_id.clone(),
                path: Some(std::path::PathBuf::from(
                    "/legacy/path/that-is-not-authoritative.jsonl",
                )),
                ..Default::default()
            },
        },
    )
    .await?;
    assert_ne!(forked.thread.id, thread_id);
    assert_eq!(forked.thread.forked_from_id, Some(thread_id.clone()));
    assert_eq!(
        forked.thread.name.as_deref(),
        Some("database native source")
    );
    assert_eq!(forked.thread.path, None);
    assert_eq!(forked.thread.status, api::ThreadStatus::Idle);
    assert_eq!(
        turn_ids_from(&forked.thread.turns),
        vec!["turn-1", "turn-2"]
    );
    assert_eq!(
        item_ids_from(&forked.thread.turns),
        vec!["user-1", "agent-1", "user-2", "agent-2"]
    );

    for (id, last_turn_id, before_turn_id) in [
        (14, Some("turn-1".to_string()), None),
        (15, None, Some("turn-2".to_string())),
    ] {
        let bounded: api::ThreadForkResponse = request(
            &client,
            api::ClientRequest::ThreadFork {
                request_id: request_id(id),
                params: api::ThreadForkParams {
                    thread_id: thread_id.clone(),
                    last_turn_id,
                    before_turn_id,
                    ..Default::default()
                },
            },
        )
        .await?;
        assert_eq!(turn_ids_from(&bounded.thread.turns), vec!["turn-1"]);
        assert_eq!(
            item_ids_from(&bounded.thread.turns),
            vec!["user-1", "agent-1"]
        );
    }

    let verifier_runtime = fixture.runtime(codex_home.path()).await?;
    let verifier = store::PostgresThreadStore::from_runtime(Arc::clone(&verifier_runtime))?;
    let forked_thread_id = ThreadId::from_string(&forked.thread.id)?;
    let persisted_fork = verifier
        .read_thread(store::ReadThreadParams {
            thread_id: forked_thread_id,
            include_archived: false,
            include_history: true,
        })
        .await?;
    assert_eq!(
        persisted_fork.forked_from_id,
        Some(ThreadId::from_string(&thread_id)?)
    );
    assert_eq!(persisted_fork.parent_thread_id, None);
    assert_eq!(
        persisted_fork.name.as_deref(),
        Some("database native source")
    );
    assert_eq!(persisted_fork.preview, "database native needle");
    assert_eq!(persisted_fork.rollout_path, None);
    assert_eq!(persisted_fork.cwd, Path::new(forked.thread.cwd.as_str()));
    assert_eq!(persisted_fork.model_provider, "mock_provider");
    assert_eq!(
        persisted_fork.history_mode,
        codex_protocol::protocol::ThreadHistoryMode::Paginated
    );
    let persisted_history = persisted_fork
        .history
        .expect("persistent fork should expose canonical PostgreSQL history");
    let fork_context = verifier
        .load_latest_model_context(store::LoadThreadHistoryParams {
            thread_id: forked_thread_id,
            include_archived: false,
        })
        .await?;
    assert!(matches!(
        fork_context.items.first(),
        Some(RolloutItem::SessionMeta(meta)) if meta.meta.id == forked_thread_id
    ));
    let foreign_context = persisted_history
        .items
        .iter()
        .find_map(|item| match item {
            RolloutItem::TurnContext(context) => Some(context),
            _ => None,
        })
        .context("foreign turn context must survive public resume and fork")?;
    let serialized_context = serde_json::to_value(foreign_context)?;
    assert_eq!(serialized_context["cwd"], json!(foreign_cwd));
    assert_eq!(serialized_context["workspace_roots"][0], json!(foreign_cwd));
    assert_eq!(
        serialized_context["sandbox_policy"]["writable_roots"][0],
        json!(foreign_cwd)
    );
    assert_eq!(
        serialized_context["permission_profile"]["file_system"]["entries"][0]["path"]["path"],
        json!(foreign_cwd)
    );
    assert_eq!(
        serialized_context["file_system_sandbox_policy"]["entries"][0]["path"]["path"],
        json!(foreign_cwd)
    );
    assert_eq!(
        foreign_context.permission_profile(),
        codex_protocol::models::PermissionProfile::read_only()
    );
    assert_dynamic_tools(&persisted_history.items);
    assert_eq!(
        persisted_history
            .items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => Some(event.turn_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["turn-1", "turn-2"]
    );

    let started: api::ThreadStartResponse = request(
        &client,
        api::ClientRequest::ThreadStart {
            request_id: request_id(/*id*/ 16),
            params: api::ThreadStartParams {
                history_mode: Some(api::ThreadHistoryMode::Paginated),
                ..Default::default()
            },
        },
    )
    .await?;
    assert_eq!(
        started.thread.history_mode,
        api::ThreadHistoryMode::Paginated
    );
    assert_eq!(started.thread.path, None);

    let continued: api::TurnStartResponse = request(
        &client,
        api::ClientRequest::TurnStart {
            request_id: request_id(/*id*/ 17),
            params: api::TurnStartParams {
                thread_id: thread_id.clone(),
                input: vec![api::UserInput::Text {
                    text: "after cross-host resume".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        },
    )
    .await?;
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let Some(event) = client.next_event().await else {
                anyhow::bail!("cross-host replica stopped before turn/completed");
            };
            if let InProcessServerEvent::ServerNotification(notification) = event
                && let api::ServerNotification::TurnCompleted(completed) = *notification
                && completed.thread_id == thread_id
                && completed.turn.id == continued.turn.id
            {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await??;
    let requests = model_server
        .received_requests()
        .await
        .context("failed to read cross-host mock model requests")?;
    let model_request = requests
        .iter()
        .rev()
        .find(|request| request.url.path().ends_with("/responses"))
        .context("missing post-resume model request")?
        .body_json::<Value>()?;
    let model_input = model_request
        .get("input")
        .and_then(Value::as_array)
        .context("post-resume model request must contain input")?;
    let serialized_input = serde_json::to_string(model_input)?;
    assert!(serialized_input.contains("database native needle"));
    assert!(serialized_input.contains("after cross-host resume"));
    assert!(!serialized_input.contains("presentation-only"));

    client.shutdown().await?;
    writer_runtime.close().await;
    reader_runtime.close().await;
    verifier_runtime.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_resume_accepts_oversized_tool_media_without_rewriting_history()
-> Result<()> {
    let fixture = PostgresFixture::new()?;
    fixture.migrate().await?;
    let model_server = create_mock_responses_server_repeating_assistant("unused").await;
    let codex_home = TempDir::new()?;
    app_test_support::MockResponsesConfig::new(&model_server.uri())
        .disable_feature(Feature::Plugins)
        .write(codex_home.path())?;
    let writer_runtime = fixture.runtime(codex_home.path()).await?;
    let writer = store::PostgresThreadStore::from_runtime(Arc::clone(&writer_runtime))?;
    let thread_id = ThreadId::new();
    seed_thread(&writer, thread_id, codex_home.path(), "mock_provider").await?;
    writer
        .resume_thread(store::ResumeThreadParams {
            thread_id,
            rollout_path: None,
            history: None,
            include_archived: false,
            metadata: store::ThreadPersistenceMetadata {
                cwd: Some(codex_home.path().to_path_buf()),
                model_provider: "mock_provider".to_string(),
                memory_mode: ThreadMemoryMode::Enabled,
            },
        })
        .await?;
    let media_call_id = "resume-limit-media-call";
    let replacement_history = vec![
        ResponseItem::CustomToolCall {
            id: None,
            status: Some("completed".to_string()),
            call_id: media_call_id.to_string(),
            name: "media_tool".to_string(),
            namespace: None,
            input: "{}".to_string(),
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: media_call_id.to_string(),
            name: Some("media_tool".to_string()),
            output: FunctionCallOutputPayload::from_content_items(
                (0..6)
                    .map(|index| FunctionCallOutputContentItem::InputImage {
                        image_url: format!("data:image/png;base64,resume-image-{index}"),
                        detail: None,
                    })
                    .collect(),
            ),
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    ];
    writer
        .append_items(store::AppendThreadItemsParams {
            thread_id,
            items: vec![RolloutItem::Compacted(CompactedItem {
                message: String::new(),
                replacement_history: Some(replacement_history.clone()),
                window_number: Some(1),
                first_window_id: Some(Uuid::now_v7().to_string()),
                previous_window_id: None,
                window_id: Some(Uuid::now_v7().to_string()),
            })],
        })
        .await?;
    writer.shutdown_thread(thread_id).await?;

    let reader_runtime = fixture.runtime(codex_home.path()).await?;
    let reader_store = Arc::new(store::PostgresThreadStore::from_runtime(Arc::clone(
        &reader_runtime,
    ))?);
    let thread_store: Arc<dyn store::ThreadStore> = reader_store.clone();
    let client = start_in_process_server_with_thread_store(codex_home.path(), thread_store).await?;
    let _: api::ThreadResumeResponse = request(
        &client,
        api::ClientRequest::ThreadResume {
            request_id: request_id(/*id*/ 99),
            params: api::ThreadResumeParams {
                thread_id: thread_id.to_string(),
                ..Default::default()
            },
        },
    )
    .await?;
    let stored = reader_store
        .read_thread(store::ReadThreadParams {
            thread_id,
            include_archived: false,
            include_history: true,
        })
        .await?;
    let persisted_replacement = stored
        .history
        .context("resumed thread should retain canonical history")?
        .items
        .into_iter()
        .find_map(|item| match item {
            RolloutItem::Compacted(compacted) => compacted.replacement_history,
            _ => None,
        })
        .context("resumed thread should retain compacted replacement history")?;
    assert_eq!(persisted_replacement, replacement_history);

    client.shutdown().await?;
    writer_runtime.close().await;
    reader_runtime.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_resume_splits_oversized_developer_bundle_without_loss() -> Result<()> {
    let fixture = PostgresFixture::new()?;
    fixture.migrate().await?;
    let model_server = create_mock_responses_server_repeating_assistant("bounded resume").await;
    let codex_home = TempDir::new()?;
    app_test_support::MockResponsesConfig::new(&model_server.uri())
        .disable_feature(Feature::Plugins)
        .write(codex_home.path())?;
    let writer_runtime = fixture.runtime(codex_home.path()).await?;
    let writer = store::PostgresThreadStore::from_runtime(Arc::clone(&writer_runtime))?;
    let thread_id = ThreadId::new();
    seed_thread(&writer, thread_id, codex_home.path(), "mock_provider").await?;
    writer
        .resume_thread(store::ResumeThreadParams {
            thread_id,
            rollout_path: None,
            history: None,
            include_archived: false,
            metadata: store::ThreadPersistenceMetadata {
                cwd: Some(codex_home.path().to_path_buf()),
                model_provider: "mock_provider".to_string(),
                memory_mode: ThreadMemoryMode::Enabled,
            },
        })
        .await?;
    let presentation_items = vec![completed_item(
        thread_id,
        "turn-2",
        TurnItem::Reasoning(codex_protocol::items::ReasoningItem {
            id: "presentation-only".to_string(),
            summary_text: Vec::new(),
            raw_content: vec!["presentation-only".repeat(1_000_000)],
        }),
    )];
    writer
        .append_items(store::AppendThreadItemsParams {
            thread_id,
            items: presentation_items,
        })
        .await?;
    let section = |label: &str, bytes: usize| {
        let prefix = format!("resume-limit-{label}:");
        format!("{prefix}{}", "x".repeat(bytes - prefix.len()))
    };
    let sections = [
        section("memory", 17_398),
        section("skills", 19_822),
        section("permissions", 362),
        section("collaboration", 966),
        section("apps", 646),
        section("plugins", 1_014),
    ];
    let oversized_developer = ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: sections
            .iter()
            .cloned()
            .map(|text| ContentItem::InputText { text })
            .collect(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let oversized_agent_text = format!("resume-limit-agent:{}", "y".repeat(45_000));
    let oversized_agent = ResponseItem::AgentMessage {
        id: None,
        author: "parent".to_string(),
        recipient: "child".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: oversized_agent_text.clone(),
        }],
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(serde_json::to_vec(&oversized_developer)?.len() > 40_000);
    assert!(serde_json::to_vec(&oversized_agent)?.len() > 40_000);
    writer
        .append_items(store::AppendThreadItemsParams {
            thread_id,
            items: vec![RolloutItem::Compacted(CompactedItem {
                message: String::new(),
                replacement_history: Some(vec![oversized_developer.into(), oversized_agent.into()]),
                window_number: Some(1),
                first_window_id: Some(Uuid::now_v7().to_string()),
                previous_window_id: None,
                window_id: Some(Uuid::now_v7().to_string()),
            })],
        })
        .await?;
    writer.shutdown_thread(thread_id).await?;
    let verification_pool = sqlx::PgPool::connect(&fixture.database_url).await?;
    let presentation_bytes: i32 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT octet_length(item::text) FROM \"{}\".thread_history \
         WHERE thread_id = $1 AND item #>> '{{payload,item,id}}' = 'presentation-only'",
        fixture.schema
    )))
    .bind(thread_id.to_string())
    .fetch_one(&verification_pool)
    .await?;
    assert!(presentation_bytes > 16 * 1024 * 1024);
    verification_pool.close().await;

    let reader_runtime = fixture.runtime(codex_home.path()).await?;
    let thread_store: Arc<dyn store::ThreadStore> = Arc::new(
        store::PostgresThreadStore::from_runtime(Arc::clone(&reader_runtime))?,
    );
    let mut client =
        start_in_process_server_with_thread_store(codex_home.path(), thread_store).await?;
    let thread_id = thread_id.to_string();
    let _: api::ThreadResumeResponse = request(
        &client,
        api::ClientRequest::ThreadResume {
            request_id: request_id(/*id*/ 100),
            params: api::ThreadResumeParams {
                thread_id: thread_id.clone(),
                ..Default::default()
            },
        },
    )
    .await?;
    let continued: api::TurnStartResponse = request(
        &client,
        api::ClientRequest::TurnStart {
            request_id: request_id(/*id*/ 101),
            params: api::TurnStartParams {
                thread_id: thread_id.clone(),
                input: vec![api::UserInput::Text {
                    text: "after bounded resume".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        },
    )
    .await?;
    timeout(DEFAULT_READ_TIMEOUT.saturating_mul(/*rhs*/ 3), async {
        loop {
            let Some(event) = client.next_event().await else {
                anyhow::bail!("resumed replica stopped before turn/completed");
            };
            if let InProcessServerEvent::ServerNotification(notification) = event
                && let api::ServerNotification::TurnCompleted(completed) = *notification
                && completed.thread_id == thread_id
                && completed.turn.id == continued.turn.id
            {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await??;
    let requests = model_server
        .received_requests()
        .await
        .context("failed to read post-resume mock model requests")?;
    let model_request = requests
        .iter()
        .rev()
        .find(|request| request.url.path().ends_with("/responses"))
        .context("missing post-resume model request")?
        .body_json::<Value>()?;
    let model_input = model_request["input"]
        .as_array()
        .context("post-resume model request must contain input")?;
    assert!(!serde_json::to_string(model_input)?.contains("presentation-only"));
    let mut replayed_sections = Vec::new();
    for item in model_input {
        assert!(
            serde_json::to_vec(item)?.len() <= 40_000,
            "post-resume model input item exceeded 10,000 estimated tokens"
        );
        if item.get("role").and_then(Value::as_str) != Some("developer") {
            continue;
        }
        for content in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(text) = content.get("text").and_then(Value::as_str)
                && text.starts_with("resume-limit-")
            {
                replayed_sections.push(text);
            }
        }
    }
    assert_eq!(
        replayed_sections,
        sections.iter().map(String::as_str).collect::<Vec<_>>()
    );
    let replayed_agent_items = model_input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("agent_message"));
    assert!(
        replayed_agent_items
            .clone()
            .all(|item| { item["author"] == "parent" && item["recipient"] == "child" })
    );
    let replayed_agent_text = replayed_agent_items
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter_map(|content| content.get("text").and_then(Value::as_str))
        .collect::<String>();
    assert_eq!(replayed_agent_text, oversized_agent_text);

    client.shutdown().await?;
    writer_runtime.close().await;
    reader_runtime.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_paginated_rollback_is_visible_across_replicas() -> Result<()> {
    let fixture = PostgresFixture::new()?;
    fixture.migrate().await?;
    let model_server =
        create_mock_responses_server_repeating_assistant("after rollback answer").await;
    let codex_home = TempDir::new()?;
    let origin_runtime = fixture.runtime(codex_home.path()).await?;
    let origin = store::PostgresThreadStore::from_runtime(Arc::clone(&origin_runtime))?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            r#"
model = "mock-model"
model_provider = "mock_provider"
approval_policy = "never"
sandbox_mode = "read-only"

[model_providers.mock_provider]
name = "Mock provider"
base_url = "{}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0

[features]
plugins = false
"#,
            model_server.uri()
        ),
    )?;
    let thread_id = ThreadId::new();
    seed_thread(&origin, thread_id, codex_home.path(), "mock_provider").await?;
    let before_rollback = origin
        .read_thread(store::ReadThreadParams {
            thread_id,
            include_archived: false,
            include_history: false,
        })
        .await?;
    origin_runtime.close().await;

    let rollback_runtime = fixture.runtime(codex_home.path()).await?;
    let rollback_store: Arc<dyn store::ThreadStore> = Arc::new(
        store::PostgresThreadStore::from_runtime(Arc::clone(&rollback_runtime))?,
    );
    let rollback_client =
        start_in_process_server_with_thread_store(codex_home.path(), rollback_store).await?;
    let resumed: api::ThreadResumeResponse = request(
        &rollback_client,
        api::ClientRequest::ThreadResume {
            request_id: request_id(/*id*/ 1),
            params: api::ThreadResumeParams {
                thread_id: thread_id.to_string(),
                ..Default::default()
            },
        },
    )
    .await?;
    assert_eq!(
        turn_ids_from(&resumed.thread.turns),
        vec!["turn-1", "turn-2"]
    );

    let rolled_back: api::ThreadRollbackResponse = request(
        &rollback_client,
        api::ClientRequest::ThreadRollback {
            request_id: request_id(/*id*/ 2),
            params: api::ThreadRollbackParams {
                thread_id: thread_id.to_string(),
                num_turns: 1,
            },
        },
    )
    .await?;
    assert_eq!(rolled_back.thread.path, None);
    assert_eq!(rolled_back.thread.status, api::ThreadStatus::Idle);
    assert_eq!(turn_ids_from(&rolled_back.thread.turns), vec!["turn-1"]);
    assert_eq!(
        item_ids_from(&rolled_back.thread.turns),
        vec!["user-1", "agent-1"]
    );
    fixture.damage_history_projections(thread_id).await?;

    let reader_runtime = fixture.runtime(codex_home.path()).await?;
    let reader_store = Arc::new(store::PostgresThreadStore::from_runtime(Arc::clone(
        &reader_runtime,
    ))?);
    let mut reader_client = start_in_process_server_with_thread_store(
        codex_home.path(),
        Arc::clone(&reader_store) as Arc<dyn store::ThreadStore>,
    )
    .await?;
    let read: api::ThreadReadResponse = request(
        &reader_client,
        api::ClientRequest::ThreadRead {
            request_id: request_id(/*id*/ 3),
            params: api::ThreadReadParams {
                thread_id: thread_id.to_string(),
                include_turns: false,
            },
        },
    )
    .await?;
    assert_eq!(read.thread.id, thread_id.to_string());
    assert_eq!(read.thread.preview, "database native needle");

    let list: api::ThreadListResponse = request(
        &reader_client,
        api::ClientRequest::ThreadList {
            request_id: request_id(/*id*/ 4),
            params: api::ThreadListParams {
                cursor: None,
                limit: Some(10),
                sort_key: Some(api::ThreadSortKey::CreatedAt),
                sort_direction: Some(api::SortDirection::Asc),
                model_providers: Some(Vec::new()),
                source_kinds: Some(vec![api::ThreadSourceKind::Exec]),
                archived: Some(false),
                is_pinned: None,
                section_id: None,
                cwd: None,
                use_state_db_only: true,
                search_term: None,
                parent_thread_id: None,
                ancestor_thread_id: None,
                project_cwd: None,
            },
        },
    )
    .await?;
    assert_eq!(list.data.len(), 1);
    assert_eq!(list.data[0].id, thread_id.to_string());
    assert_eq!(list.data[0].preview, "database native needle");

    let turns: api::ThreadTurnsListResponse = request(
        &reader_client,
        api::ClientRequest::ThreadTurnsList {
            request_id: request_id(/*id*/ 5),
            params: api::ThreadTurnsListParams {
                thread_id: thread_id.to_string(),
                cursor: None,
                limit: Some(10),
                sort_direction: Some(api::SortDirection::Asc),
                items_view: Some(api::TurnItemsView::Full),
            },
        },
    )
    .await?;
    assert_eq!(turn_ids(&turns), vec!["turn-1"]);
    assert_eq!(item_ids_from(&turns.data), vec!["user-1", "agent-1"]);

    let items: api::ThreadItemsListResponse = request(
        &reader_client,
        api::ClientRequest::ThreadItemsList {
            request_id: request_id(/*id*/ 6),
            params: api::ThreadItemsListParams {
                thread_id: thread_id.to_string(),
                turn_id: None,
                cursor: None,
                limit: Some(10),
                sort_direction: Some(api::SortDirection::Asc),
            },
        },
    )
    .await?;
    assert_eq!(item_ids(&items), vec!["user-1", "agent-1"]);

    let search: api::ThreadSearchResponse = request(
        &reader_client,
        api::ClientRequest::ThreadSearch {
            request_id: request_id(/*id*/ 7),
            params: api::ThreadSearchParams {
                cursor: None,
                limit: Some(10),
                sort_key: None,
                sort_direction: None,
                source_kinds: None,
                archived: Some(false),
                search_term: "second needle".to_string(),
            },
        },
    )
    .await?;
    assert_eq!(search.data, Vec::new());
    let occurrences: api::ThreadSearchOccurrencesResponse = request(
        &reader_client,
        api::ClientRequest::ThreadSearchOccurrences {
            request_id: request_id(/*id*/ 8),
            params: api::ThreadSearchOccurrencesParams {
                thread_id: thread_id.to_string(),
                search_term: "second needle".to_string(),
                cursor: None,
                limit: Some(10),
            },
        },
    )
    .await?;
    assert_eq!(occurrences.data, Vec::new());

    let persisted = reader_store
        .read_thread(store::ReadThreadParams {
            thread_id,
            include_archived: false,
            include_history: true,
        })
        .await?;
    assert_eq!(persisted.preview, before_rollback.preview);
    assert_eq!(persisted.name, before_rollback.name);
    assert_eq!(persisted.recency_at, before_rollback.recency_at);
    assert!(persisted.updated_at >= before_rollback.updated_at);
    let canonical = persisted
        .history
        .context("rolled-back thread should retain canonical history")?;
    assert_dynamic_tools(&canonical.items);
    assert_eq!(
        canonical
            .items
            .iter()
            .filter(|item| matches!(item, RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_))))
            .count(),
        1
    );
    assert!(canonical.items.iter().any(|item| matches!(
        item,
        RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) if event.turn_id == "turn-2"
    )));

    rollback_client.shutdown().await?;
    rollback_runtime.close().await;
    let resumed: api::ThreadResumeResponse = request(
        &reader_client,
        api::ClientRequest::ThreadResume {
            request_id: request_id(/*id*/ 9),
            params: api::ThreadResumeParams {
                thread_id: thread_id.to_string(),
                ..Default::default()
            },
        },
    )
    .await?;
    assert_eq!(turn_ids_from(&resumed.thread.turns), vec!["turn-1"]);
    let started: api::TurnStartResponse = request(
        &reader_client,
        api::ClientRequest::TurnStart {
            request_id: request_id(/*id*/ 10),
            params: api::TurnStartParams {
                thread_id: thread_id.to_string(),
                input: vec![api::UserInput::Text {
                    text: "after rollback".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        },
    )
    .await?;
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let Some(event) = reader_client.next_event().await else {
                anyhow::bail!("reader replica stopped before turn/completed");
            };
            if let InProcessServerEvent::ServerNotification(notification) = event
                && let api::ServerNotification::TurnCompleted(completed) = *notification
                && completed.thread_id == thread_id.to_string()
            {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await??;
    let requests = model_server
        .received_requests()
        .await
        .context("failed to read mock model requests")?;
    let model_request = requests
        .iter()
        .rev()
        .find(|request| request.url.path().ends_with("/responses"))
        .context("missing post-rollback model request")?;
    let model_request = model_request.body_json::<Value>()?;
    let model_input = model_request
        .get("input")
        .context("model request should include input")?
        .to_string();
    assert!(model_input.contains("database native needle"));
    assert!(model_input.contains("after rollback"));
    assert!(!model_input.contains("second needle"));
    assert_eq!(
        dynamic_namespaces(&model_request),
        vec![
            json!({
                "type": "namespace",
                "name": "alpha",
                "description": "Alpha tools",
                "tools": [{
                    "type": "function",
                    "name": "lookup",
                    "description": "Look up by ticketId",
                    "strict": false,
                    "parameters": {
                        "type": "object",
                        "properties": { "ticketId": { "type": "string" } },
                        "required": ["ticketId"],
                        "additionalProperties": false
                    }
                }]
            }),
            json!({
                "type": "namespace",
                "name": "beta",
                "description": "Beta tools",
                "tools": [{
                    "type": "function",
                    "name": "lookup",
                    "description": "Look up by repository",
                    "strict": false,
                    "parameters": {
                        "type": "object",
                        "properties": { "repository": { "type": "string" } },
                        "required": ["repository"],
                        "additionalProperties": false
                    }
                }]
            }),
        ]
    );
    let after_append: api::ThreadTurnsListResponse = request(
        &reader_client,
        api::ClientRequest::ThreadTurnsList {
            request_id: request_id(/*id*/ 11),
            params: api::ThreadTurnsListParams {
                thread_id: thread_id.to_string(),
                cursor: None,
                limit: Some(10),
                sort_direction: Some(api::SortDirection::Asc),
                items_view: Some(api::TurnItemsView::Full),
            },
        },
    )
    .await?;
    assert_eq!(
        turn_ids(&after_append),
        vec!["turn-1", started.turn.id.as_str()]
    );
    assert!(!codex_home.path().join("sessions").exists());

    reader_client.shutdown().await?;
    reader_runtime.close().await;
    fixture.cleanup().await
}

pub(super) async fn request<T: DeserializeOwned>(
    client: &InProcessClientHandle,
    request: api::ClientRequest,
) -> Result<T> {
    let method = request.method_name();
    let response = client
        .request(request)
        .await?
        .map_err(|error| anyhow::anyhow!("{method} failed: {}", error.message))?;
    Ok(serde_json::from_value(response)?)
}

fn request_id(id: i64) -> api::RequestId {
    api::RequestId::Integer(id)
}

async fn items_page(
    client: &InProcessClientHandle,
    thread_id: &str,
    cursor: Option<String>,
) -> Result<api::ThreadItemsListResponse> {
    request(
        client,
        api::ClientRequest::ThreadItemsList {
            request_id: request_id(if cursor.is_some() { 6 } else { 5 }),
            params: api::ThreadItemsListParams {
                thread_id: thread_id.to_string(),
                turn_id: None,
                cursor,
                limit: Some(2),
                sort_direction: Some(api::SortDirection::Asc),
            },
        },
    )
    .await
}

async fn occurrence_page(
    client: &InProcessClientHandle,
    thread_id: &str,
    cursor: Option<String>,
) -> Result<api::ThreadSearchOccurrencesResponse> {
    request(
        client,
        api::ClientRequest::ThreadSearchOccurrences {
            request_id: request_id(if cursor.is_some() { 9 } else { 8 }),
            params: api::ThreadSearchOccurrencesParams {
                thread_id: thread_id.to_string(),
                search_term: "needle".to_string(),
                cursor,
                limit: Some(2),
            },
        },
    )
    .await
}

fn turn_ids(page: &api::ThreadTurnsListResponse) -> Vec<&str> {
    turn_ids_from(&page.data)
}

fn turn_ids_from(turns: &[api::Turn]) -> Vec<&str> {
    turns.iter().map(|turn| turn.id.as_str()).collect()
}

fn item_ids_from(turns: &[api::Turn]) -> Vec<&str> {
    turns
        .iter()
        .flat_map(|turn| turn.items.iter().map(api::ThreadItem::id))
        .collect()
}

fn item_ids(page: &api::ThreadItemsListResponse) -> Vec<&str> {
    page.data.iter().map(|entry| entry.item.id()).collect()
}

fn occurrence_ids(page: &api::ThreadSearchOccurrencesResponse) -> Vec<&str> {
    page.data.iter().map(|item| item.item_id.as_str()).collect()
}

fn dynamic_namespaces(body: &Value) -> Vec<Value> {
    body.get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| {
            matches!(
                tool.get("name").and_then(Value::as_str),
                Some("alpha" | "beta")
            )
        })
        .cloned()
        .collect()
}

fn assert_dynamic_tools(items: &[RolloutItem]) {
    let Some(RolloutItem::SessionMeta(session_meta)) = items.first() else {
        panic!("canonical history should start with session metadata");
    };
    assert_eq!(
        session_meta.meta.dynamic_tools,
        Some(dynamic_tools_fixture())
    );
}

async fn seed_thread(
    store: &store::PostgresThreadStore,
    thread_id: ThreadId,
    cwd: &Path,
    model_provider: &str,
) -> Result<()> {
    store
        .create_thread(store::CreateThreadParams {
            session_id: thread_id.into(),
            thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Exec,
            thread_source: None,
            originator: "app-server-postgres-contract".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: dynamic_tools_fixture(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: codex_protocol::protocol::ThreadHistoryMode::Paginated,
            history_base: None,
            subagent_history_start_ordinal: None,
            initial_window_id: Uuid::now_v7().to_string(),
            metadata: store::ThreadPersistenceMetadata {
                cwd: Some(cwd.to_path_buf()),
                model_provider: model_provider.to_string(),
                memory_mode: ThreadMemoryMode::Enabled,
            },
        })
        .await?;
    store
        .persist_thread(thread_id, PersistContext::Standard)
        .await?;
    store
        .append_items(store::AppendThreadItemsParams {
            thread_id,
            items: history(thread_id),
        })
        .await?;
    store
        .update_thread_metadata(store::UpdateThreadMetadataParams {
            thread_id,
            patch: store::ThreadMetadataPatch {
                name: Some(Some("database native source".to_string())),
                ..Default::default()
            },
            include_archived: false,
        })
        .await?;
    store.shutdown_thread(thread_id).await?;
    Ok(())
}

fn history(thread_id: ThreadId) -> Vec<RolloutItem> {
    [
        turn(
            thread_id,
            "turn-1",
            "user-1",
            "database native needle",
            "agent-1",
            /*started_at*/ 10,
        ),
        turn(
            thread_id,
            "turn-2",
            "user-2",
            "second needle",
            "agent-2",
            /*started_at*/ 30,
        ),
    ]
    .concat()
}

fn dynamic_tools_fixture() -> Vec<api::DynamicToolSpec> {
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
) -> api::DynamicToolSpec {
    api::DynamicToolSpec::Namespace(api::DynamicToolNamespaceSpec {
        name: name.to_string(),
        description: description.to_string(),
        tools: vec![
            api::DynamicToolNamespaceTool::Function(api::DynamicToolFunctionSpec {
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
            api::DynamicToolNamespaceTool::Function(api::DynamicToolFunctionSpec {
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

fn turn(
    thread_id: ThreadId,
    turn_id: &str,
    user_id: &str,
    user_text: &str,
    agent_id: &str,
    started_at: i64,
) -> Vec<RolloutItem> {
    vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: turn_id.to_string(),
            trace_id: None,
            started_at: Some(started_at),
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        completed_item(
            thread_id,
            turn_id,
            TurnItem::UserMessage(UserMessageItem {
                id: user_id.to_string(),
                client_id: None,
                content: vec![UserInput::Text {
                    text: user_text.to_string(),
                    text_elements: Vec::new(),
                }],
            }),
        ),
        RolloutItem::ResponseItem(
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: user_text.to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
            .into(),
        ),
        completed_item(
            thread_id,
            turn_id,
            TurnItem::AgentMessage(AgentMessageItem {
                id: agent_id.to_string(),
                content: vec![AgentMessageContent::Text {
                    text: format!("final {user_text}"),
                }],
                phase: Some(MessagePhase::FinalAnswer),
                memory_citation: None,
            }),
        ),
        RolloutItem::ResponseItem(
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: format!("final {user_text}"),
                }],
                phase: Some(MessagePhase::FinalAnswer),
                internal_chat_message_metadata_passthrough: None,
            }
            .into(),
        ),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: turn_id.to_string(),
            last_agent_message: None,
            error: None,
            started_at: Some(started_at),
            completed_at: Some(started_at + 10),
            duration_ms: Some(10_000),
            time_to_first_token_ms: None,
        })),
    ]
}

fn completed_item(thread_id: ThreadId, turn_id: &str, item: TurnItem) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: turn_id.to_string(),
        item,
        started_at_ms: Some(0),
        completed_at_ms: 1_000,
    }))
}

pub(super) struct PostgresFixture {
    config: PostgresNamespaceConfig,
    pub(super) database_url: String,
    pub(super) schema: String,
}

impl PostgresFixture {
    pub(super) fn new() -> Result<Self> {
        let database_url = std::env::var(DATABASE_URL_ENV)?;
        let schema = format!("codex_app_server_{}", Uuid::new_v4().simple());
        let config = PostgresNamespaceConfig::new(
            DATABASE_URL_ENV.to_string(),
            schema.clone(),
            PostgresPoolConfig::default(),
        )?;
        Ok(Self {
            config,
            database_url,
            schema,
        })
    }

    pub(super) async fn migrate(&self) -> Result<()> {
        codex_state::manage_postgres_namespace(
            self.config.clone(),
            PostgresNamespaceAction::Migrate,
        )
        .await?;
        let pool = sqlx::PgPool::connect(&self.database_url).await?;
        let migration = format!("\"{}\".runtime_state_migration", self.schema);
        let evidence = json!({
            "sourceIdentity": "app-server-thread-store-contract",
            "sourceFingerprint": "app-server-thread-store-contract-fingerprint",
            "phase": "ready",
            "ready": true,
            "fencingToken": 4,
            "namespaceDigest": "app-server-thread-store-contract-digest",
            "globalReferentialIntegrityValidated": true,
            "canonicalThreadHistoryOrderingValidated": true,
        });
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {migration} (source_identity, source_fingerprint, phase, ready, \
             phase_evidence, fencing_token) VALUES ($1, $2, 'ready', TRUE, $3, 4)"
        )))
        .bind("app-server-thread-store-contract")
        .bind("app-server-thread-store-contract-fingerprint")
        .bind(evidence)
        .execute(&pool)
        .await?;
        pool.close().await;
        Ok(())
    }

    pub(super) async fn runtime(&self, codex_home: &Path) -> Result<Arc<StateRuntime>> {
        StateRuntime::init_with_backend(
            RuntimeStateBackendConfig::Postgresql {
                codex_home: AbsolutePathBuf::try_from(codex_home.to_path_buf())?,
                namespace: self.config.clone(),
            },
            "openai".to_string(),
        )
        .await
    }

    async fn damage_history_projections(&self, thread_id: ThreadId) -> Result<()> {
        let pool = sqlx::PgPool::connect(&self.database_url).await?;
        let schema = &self.schema;
        let mut transaction = pool.begin().await?;
        for table in ["thread_items", "thread_turns", "thread_search_content"] {
            sqlx::query(AssertSqlSafe(format!(
                "DELETE FROM \"{schema}\".{table} WHERE thread_id = $1"
            )))
            .bind(thread_id.to_string())
            .execute(transaction.as_mut())
            .await?;
        }
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE \"{schema}\".threads SET history_projection_version = NULL \
             WHERE thread_id = $1"
        )))
        .bind(thread_id.to_string())
        .execute(transaction.as_mut())
        .await?;
        let checkpoint = sqlx::query_scalar::<_, Option<i64>>(AssertSqlSafe(format!(
            "SELECT history_projection_version FROM \"{schema}\".threads WHERE thread_id = $1"
        )))
        .bind(thread_id.to_string())
        .fetch_one(transaction.as_mut())
        .await?;
        assert_eq!(checkpoint, None);
        transaction.commit().await?;
        pool.close().await;
        Ok(())
    }

    async fn append_foreign_host_context(
        &self,
        thread_id: ThreadId,
        cwd: &Path,
        writer: &store::PostgresThreadStore,
    ) -> Result<&'static str> {
        let foreign_cwd = if cfg!(windows) {
            "/home/k3/git/scrape-golden"
        } else {
            r"C:\Users\msilvane\git\scrape-golden"
        };
        let template = TurnContextItem {
            turn_id: Some("foreign-host-context".to_string()),
            cwd: AbsolutePathBuf::try_from(cwd.to_path_buf())?.into(),
            workspace_roots: None,
            current_date: None,
            timezone: None,
            approval_policy: codex_protocol::protocol::AskForApproval::Never,
            approvals_reviewer: None,
            sandbox_policy: codex_protocol::protocol::SandboxPolicy::DangerFullAccess.into(),
            permission_profile: None,
            network: None,
            file_system_sandbox_policy: None,
            model: "mock-model".to_string(),
            comp_hash: None,
            personality: None,
            collaboration_mode: None,
            multi_agent_version: None,
            multi_agent_mode: None,
            realtime_active: None,
            effort: None,
            summary: codex_protocol::config_types::ReasoningSummary::Auto,
        };
        writer
            .resume_thread(store::ResumeThreadParams {
                thread_id,
                rollout_path: None,
                history: None,
                include_archived: false,
                metadata: store::ThreadPersistenceMetadata {
                    cwd: Some(cwd.to_path_buf()),
                    model_provider: "openai".to_string(),
                    memory_mode: ThreadMemoryMode::Enabled,
                },
            })
            .await?;
        writer
            .append_items(store::AppendThreadItemsParams {
                thread_id,
                items: vec![RolloutItem::TurnContext(template)],
            })
            .await?;
        writer.shutdown_thread(thread_id).await?;
        let foreign_context = json!({
            "type": "turn_context",
            "payload": {
                "turn_id": "foreign-host-context",
                "cwd": foreign_cwd,
                "workspace_roots": [foreign_cwd],
                "approval_policy": "never",
                "sandbox_policy": {
                    "type": "workspace-write",
                    "writable_roots": [foreign_cwd],
                    "network_access": false,
                    "exclude_tmpdir_env_var": false,
                    "exclude_slash_tmp": false
                },
                "permission_profile": {
                    "type": "managed",
                    "file_system": {
                        "type": "restricted",
                        "entries": [{
                            "path": { "type": "path", "path": foreign_cwd },
                            "access": "write"
                        }]
                    },
                    "network": "restricted"
                },
                "file_system_sandbox_policy": {
                    "kind": "restricted",
                    "entries": [{
                        "path": { "type": "path", "path": foreign_cwd },
                        "access": "write"
                    }]
                },
                "model": "mock-model",
                "summary": "auto"
            }
        });
        let foreign_permission_profile = foreign_context["payload"]["permission_profile"].clone();
        let pool = sqlx::PgPool::connect(&self.database_url).await?;
        let mut transaction = pool.begin().await?;
        let schema = &self.schema;
        let large_presentation_item =
            RolloutItem::EventMsg(EventMsg::Warning(codex_protocol::protocol::WarningEvent {
                message: "presentation-only".repeat(20_000),
            }));
        let presentation_bytes: i32 = sqlx::query_scalar(AssertSqlSafe(format!(
            "INSERT INTO \"{schema}\".thread_history (thread_id, ordinal, item) \
             SELECT $1, COALESCE(MAX(ordinal), -1) + 1, $2 \
             FROM \"{schema}\".thread_history WHERE thread_id = $1 \
             RETURNING octet_length(item::text)"
        )))
        .bind(thread_id.to_string())
        .bind(serde_json::to_value(large_presentation_item)?)
        .fetch_one(transaction.as_mut())
        .await?;
        assert!(presentation_bytes > 40_000);
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE \"{schema}\".threads \
             SET stream_version = stream_version + 1, history_projection_version = NULL \
             WHERE thread_id = $1"
        )))
        .bind(thread_id.to_string())
        .execute(transaction.as_mut())
        .await?;
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE \"{schema}\".thread_history \
             SET item = $2 \
             WHERE thread_id = $1 \
               AND item ->> 'type' = 'turn_context' \
               AND item #>> '{{payload,turn_id}}' = 'foreign-host-context'"
        )))
        .bind(thread_id.to_string())
        .bind(foreign_context)
        .execute(transaction.as_mut())
        .await?;
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE \"{schema}\".thread_history \
             SET item = jsonb_set(item, '{{payload,cwd}}', to_jsonb($2::text), false) \
             WHERE thread_id = $1 AND ordinal = 0"
        )))
        .bind(thread_id.to_string())
        .bind(foreign_cwd)
        .execute(transaction.as_mut())
        .await?;
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE \"{schema}\".threads \
             SET projection = jsonb_set(\
                 jsonb_set(projection, '{{cwd}}', to_jsonb($2::text), false), \
                 '{{permission_profile}}', $3, false\
             ) \
             WHERE thread_id = $1"
        )))
        .bind(thread_id.to_string())
        .bind(foreign_cwd)
        .bind(foreign_permission_profile)
        .execute(transaction.as_mut())
        .await?;
        transaction.commit().await?;
        pool.close().await;
        Ok(foreign_cwd)
    }

    pub(super) async fn cleanup(&self) -> Result<()> {
        let pool = sqlx::PgPool::connect(&self.database_url).await?;
        sqlx::query(AssertSqlSafe(format!(
            "DROP SCHEMA \"{}\" CASCADE",
            self.schema
        )))
        .execute(&pool)
        .await?;
        pool.close().await;
        Ok(())
    }
}
