use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use codex_app_server::in_process::InProcessClientHandle;
use codex_app_server_protocol as api;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::user_input::UserInput;
use codex_state::PostgresNamespaceAction;
use codex_state::PostgresNamespaceConfig;
use codex_state::PostgresPoolConfig;
use codex_state::PostgresRuntimeStatePool;
use codex_thread_store as store;
use codex_thread_store::ThreadStore;
use pretty_assertions::assert_eq;
use serde::de::DeserializeOwned;
use sqlx::AssertSqlSafe;
use tempfile::TempDir;
use uuid::Uuid;

use super::remote_thread_store::start_in_process_server_with_thread_store;

const DATABASE_URL_ENV: &str = "CODEX_TEST_POSTGRES_URL";

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_store_serves_database_native_v2_history_flows() -> Result<()> {
    let fixture = PostgresFixture::new()?;
    fixture.migrate().await?;
    let writer_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let reader_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let writer = store::PostgresThreadStore::new(&writer_pool);
    let codex_home = TempDir::new()?;
    let thread_id = ThreadId::new();
    seed_thread(&writer, thread_id, codex_home.path()).await?;

    let thread_store: Arc<dyn store::ThreadStore> =
        Arc::new(store::PostgresThreadStore::new(&reader_pool));
    let client = start_in_process_server_with_thread_store(codex_home.path(), thread_store).await?;
    let thread_id = thread_id.to_string();

    let list: api::ThreadListResponse = request(
        &client,
        api::ClientRequest::ThreadList {
            request_id: request_id(1),
            params: api::ThreadListParams {
                cursor: None,
                limit: Some(1),
                sort_key: Some(api::ThreadSortKey::CreatedAt),
                sort_direction: Some(api::SortDirection::Asc),
                model_providers: Some(Vec::new()),
                source_kinds: Some(vec![api::ThreadSourceKind::Exec]),
                archived: Some(false),
                cwd: None,
                use_state_db_only: true,
                search_term: None,
                parent_thread_id: None,
                ancestor_thread_id: None,
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
            request_id: request_id(2),
            params: api::ThreadReadParams {
                thread_id: thread_id.clone(),
                include_turns: false,
            },
        },
    )
    .await?;
    assert_eq!(read.thread.id, thread_id);
    assert_eq!(read.thread.preview, "database native needle");

    let first_turn: api::ThreadTurnsListResponse = request(
        &client,
        api::ClientRequest::ThreadTurnsList {
            request_id: request_id(3),
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
            request_id: request_id(4),
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

    let first_items = items_page(&client, &thread_id, None).await?;
    assert_eq!(item_ids(&first_items), vec!["user-1", "agent-1"]);
    let second_items = items_page(&client, &thread_id, first_items.next_cursor).await?;
    assert_eq!(item_ids(&second_items), vec!["user-2", "agent-2"]);

    let search: api::ThreadSearchResponse = request(
        &client,
        api::ClientRequest::ThreadSearch {
            request_id: request_id(7),
            params: api::ThreadSearchParams {
                cursor: None,
                limit: Some(1),
                sort_key: Some(api::ThreadSortKey::CreatedAt),
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

    let first_occurrences = occurrence_page(&client, &thread_id, None).await?;
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
            request_id: request_id(10),
            params: api::ThreadResumeParams {
                thread_id: thread_id.clone(),
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

    let started: api::ThreadStartResponse = request(
        &client,
        api::ClientRequest::ThreadStart {
            request_id: request_id(11),
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

    client.shutdown().await?;
    writer_pool.close().await;
    reader_pool.close().await;
    fixture.cleanup().await
}

async fn request<T: DeserializeOwned>(
    client: &InProcessClientHandle,
    request: api::ClientRequest,
) -> Result<T> {
    let method = request.method();
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

fn item_ids(page: &api::ThreadItemsListResponse) -> Vec<&str> {
    page.data.iter().map(|entry| entry.item.id()).collect()
}

fn occurrence_ids(page: &api::ThreadSearchOccurrencesResponse) -> Vec<&str> {
    page.data.iter().map(|item| item.item_id.as_str()).collect()
}

async fn seed_thread(
    store: &store::PostgresThreadStore,
    thread_id: ThreadId,
    cwd: &Path,
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
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: codex_protocol::protocol::ThreadHistoryMode::Paginated,
            subagent_history_start_ordinal: None,
            initial_window_id: Uuid::now_v7().to_string(),
            metadata: store::ThreadPersistenceMetadata {
                cwd: Some(cwd.to_path_buf()),
                model_provider: "openai".to_string(),
                memory_mode: ThreadMemoryMode::Enabled,
            },
        })
        .await?;
    store.persist_thread(thread_id).await?;
    store
        .append_items(store::AppendThreadItemsParams {
            thread_id,
            items: history(thread_id),
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
            10,
        ),
        turn(
            thread_id,
            "turn-2",
            "user-2",
            "second needle",
            "agent-2",
            30,
        ),
    ]
    .concat()
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
        completed_at_ms: 1_000,
    }))
}

struct PostgresFixture {
    config: PostgresNamespaceConfig,
    database_url: String,
    schema: String,
}

impl PostgresFixture {
    fn new() -> Result<Self> {
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

    async fn migrate(&self) -> Result<()> {
        codex_state::manage_postgres_namespace(
            self.config.clone(),
            PostgresNamespaceAction::Migrate,
        )
        .await?;
        Ok(())
    }

    async fn cleanup(&self) -> Result<()> {
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
