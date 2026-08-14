#![allow(
    clippy::disallowed_methods,
    reason = "PostgreSQL tests connect only to PostgreSQL pools"
)]

use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;

use crate::AppendBatchId;
use crate::AppendThreadItemsBatch;
use crate::ArchiveThreadParams;
use crate::ItemSortKey;
use crate::ListItemsParams;
use crate::ListTurnsParams;
use crate::LoadThreadHistoryParams;
use crate::PostgresThreadStore;
use crate::SortDirection;
use crate::StoredTurn;
use crate::StoredTurnItemsView;
use crate::StoredTurnStatus;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::postgres_contract_tests::PostgresThreadStoreFixture;
use crate::postgres_contract_tests::create_thread_params;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_damaged_projections_rebuild_once_for_concurrent_public_readers()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_projection_rebuild")?;
    fixture.migrate().await?;
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let first_reader = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let second_reader = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f025")?;
    let mut create_params = create_thread_params(thread_id);
    create_params.history_mode = ThreadHistoryMode::Paginated;
    writer.create_thread(create_params).await?;
    writer
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            paginated_turn_history(thread_id),
        ))
        .await?;
    damage_history_projections(&fixture, thread_id).await?;

    let (items, turns) = tokio::join!(
        first_reader.list_items(ListItemsParams {
            thread_id,
            turn_id: None,
            include_archived: false,
            cursor: None,
            page_size: 10,
            sort_direction: SortDirection::Asc,
            sort_key: ItemSortKey::CreatedAtOrdinal,
            after_updated_at_ordinal: None,
        }),
        second_reader.list_turns(default_turn_params(thread_id)),
    );
    assert_eq!(
        items?
            .items
            .into_iter()
            .map(|item| item.item_id)
            .collect::<Vec<_>>(),
        vec!["user-1", "agent-1"]
    );
    assert_eq!(turn_ids(&turns?), vec!["turn-1", "turn-2", "turn-3"]);

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_turn_pages_match_local_summary_and_bidirectional_pagination()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_turn_pages")?;
    fixture.migrate().await?;
    let writer_pool = fixture.connect_pool().await?;
    let reader_pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(writer_pool.clone(), fixture.schema.clone());
    let reader = PostgresThreadStore::new(reader_pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f022")?;
    let mut create_params = create_thread_params(thread_id);
    create_params.history_mode = ThreadHistoryMode::Paginated;
    writer.create_thread(create_params).await?;
    writer
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331ba22")?,
            paginated_turn_history(thread_id),
        ))
        .await?;

    let first_page = reader
        .list_turns(turn_params(
            thread_id,
            /*cursor*/ None,
            /*page_size*/ 2,
            SortDirection::Asc,
            StoredTurnItemsView::Summary,
        ))
        .await?;
    assert_eq!(turn_ids(&first_page), vec!["turn-1", "turn-2"]);
    assert_eq!(item_ids(&first_page.turns[0]), vec!["user-1", "agent-1"]);
    assert_eq!(first_page.turns[0].status, StoredTurnStatus::Completed);
    assert_eq!(first_page.turns[0].started_at, Some(10));
    assert_eq!(first_page.turns[0].completed_at, Some(20));
    assert_eq!(first_page.turns[0].duration_ms, Some(10_000));
    let final_agent =
        serde_json::from_slice::<serde_json::Value>(&first_page.turns[0].items[1].item_json)?;
    assert_eq!(final_agent["text"], "final");
    assert_eq!(final_agent["phase"], "final_answer");
    let error = first_page.turns[1]
        .error
        .as_ref()
        .expect("failed turn error");
    assert_eq!(error.message, "request failed");

    let second_page = reader
        .list_turns(turn_params(
            thread_id,
            first_page.next_cursor,
            /*page_size*/ 2,
            SortDirection::Asc,
            StoredTurnItemsView::NotLoaded,
        ))
        .await?;
    assert_eq!(turn_ids(&second_page), vec!["turn-3"]);
    assert_eq!(second_page.turns[0].status, StoredTurnStatus::InProgress);
    assert_eq!(second_page.turns[0].items, Vec::new());
    let backwards_page = reader
        .list_turns(turn_params(
            thread_id,
            second_page.backwards_cursor,
            /*page_size*/ 2,
            SortDirection::Desc,
            StoredTurnItemsView::NotLoaded,
        ))
        .await?;
    assert_eq!(turn_ids(&backwards_page), vec!["turn-3", "turn-2"]);

    let mut invalid_cursor = default_turn_params(thread_id);
    invalid_cursor.cursor = Some("{}".to_string());
    assert_invalid(&reader, invalid_cursor).await;
    assert_unsupported(&reader, ThreadId::default()).await;
    let legacy_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f023")?;
    writer
        .create_thread(create_thread_params(legacy_id))
        .await?;
    assert_unsupported(&reader, legacy_id).await;
    writer.shutdown_thread(thread_id).await?;
    writer
        .archive_thread(ArchiveThreadParams { thread_id })
        .await?;
    assert_invalid(&reader, default_turn_params(thread_id)).await;
    let mut archived_params = default_turn_params(thread_id);
    archived_params.include_archived = true;
    assert_eq!(
        turn_ids(&reader.list_turns(archived_params).await?),
        vec!["turn-1", "turn-2", "turn-3"]
    );

    writer_pool.close().await;
    reader_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_inherited_prefix_is_excluded_and_turn_projection_failure_rolls_back()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_turn_atomicity")?;
    fixture.migrate().await?;
    let runtime_pool = fixture.connect_pool().await?;
    let store = PostgresThreadStore::new(runtime_pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f024")?;
    let mut create_params = create_thread_params(thread_id);
    create_params.history_mode = ThreadHistoryMode::Paginated;
    create_params.subagent_history_start_ordinal = Some(3);
    store.create_thread(create_params).await?;
    let history = [
        completed_turn_history(
            "inherited",
            /*started_at*/ 10,
            /*completed_at*/ 20,
        ),
        completed_turn_history("owned", /*started_at*/ 30, /*completed_at*/ 40),
    ]
    .concat();
    store
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            history,
        ))
        .await?;
    damage_history_projections(&fixture, thread_id).await?;
    assert_eq!(
        turn_ids(&store.list_turns(default_turn_params(thread_id)).await?),
        vec!["owned"]
    );
    let fault_pool = sqlx::PgPool::connect(&fixture.database_url).await?;
    let turns = format!("\"{}\".thread_turns", fixture.schema);
    sqlx::query(AssertSqlSafe(format!(
        "ALTER TABLE {turns} ADD CONSTRAINT reject_turn_projection CHECK (false) NOT VALID"
    )))
    .execute(&fault_pool)
    .await?;
    let retry_batch = AppendThreadItemsBatch::new(
        thread_id,
        AppendBatchId::new(),
        completed_turn_history("retry", /*started_at*/ 50, /*completed_at*/ 60),
    );
    assert!(matches!(
        store.append_batch(retry_batch.clone()).await,
        Err(ThreadStoreError::Internal { .. })
    ));
    sqlx::query(AssertSqlSafe(format!(
        "ALTER TABLE {turns} DROP CONSTRAINT reject_turn_projection"
    )))
    .execute(&fault_pool)
    .await?;
    assert_eq!(
        store
            .load_history(LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await?
            .items
            .len(),
        5
    );
    store.append_batch(retry_batch).await?;
    assert_eq!(
        turn_ids(&store.list_turns(default_turn_params(thread_id)).await?),
        vec!["owned", "retry"]
    );

    fault_pool.close().await;
    runtime_pool.close().await;
    fixture.cleanup().await
}

fn completed_turn_history(turn_id: &str, started_at: i64, completed_at: i64) -> Vec<RolloutItem> {
    vec![
        turn_started(turn_id, started_at),
        turn_complete(turn_id, started_at, completed_at, /*error*/ None),
    ]
}

fn paginated_turn_history(thread_id: ThreadId) -> Vec<RolloutItem> {
    let mut history = vec![
        completed_item(thread_id, "turn-1", user_item("user-1")),
        completed_item(
            thread_id,
            "turn-1",
            agent_item("agent-1", "draft", /*phase*/ None),
        ),
        completed_item(
            thread_id,
            "turn-1",
            agent_item("agent-1", "final", Some(MessagePhase::FinalAnswer)),
        ),
    ];
    history.extend(completed_turn_history(
        "turn-1", /*started_at*/ 10, /*completed_at*/ 20,
    ));
    history.push(turn_started("turn-2", /*started_at*/ 30));
    history.push(turn_complete(
        "turn-2",
        /*started_at*/ 30,
        /*completed_at*/ 40,
        Some(ErrorEvent {
            message: "request failed".to_string(),
            codex_error_info: Some(CodexErrorInfo::ServerOverloaded),
        }),
    ));
    history.push(turn_started("turn-3", /*started_at*/ 50));
    history
}

pub(super) fn turn_started(turn_id: &str, started_at: i64) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.to_string(),
        trace_id: None,
        started_at: Some(started_at),
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    }))
}

pub(super) fn turn_complete(
    turn_id: &str,
    started_at: i64,
    completed_at: i64,
    error: Option<ErrorEvent>,
) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.to_string(),
        last_agent_message: None,
        error,
        started_at: Some(started_at),
        completed_at: Some(completed_at),
        duration_ms: Some((completed_at - started_at) * 1_000),
        time_to_first_token_ms: None,
    }))
}

pub(super) fn completed_item(thread_id: ThreadId, turn_id: &str, item: TurnItem) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: turn_id.to_string(),
        item,
        started_at_ms: Some(0),
        completed_at_ms: 1_000,
    }))
}

fn user_item(item_id: &str) -> TurnItem {
    TurnItem::UserMessage(UserMessageItem {
        id: item_id.to_string(),
        client_id: None,
        content: Vec::new(),
    })
}

pub(super) fn agent_item(item_id: &str, text: &str, phase: Option<MessagePhase>) -> TurnItem {
    TurnItem::AgentMessage(AgentMessageItem {
        id: item_id.to_string(),
        content: vec![AgentMessageContent::Text {
            text: text.to_string(),
        }],
        phase,
        memory_citation: None,
    })
}

fn turn_params(
    thread_id: ThreadId,
    cursor: Option<String>,
    page_size: usize,
    sort_direction: SortDirection,
    items_view: StoredTurnItemsView,
) -> ListTurnsParams {
    ListTurnsParams {
        thread_id,
        include_archived: false,
        cursor,
        page_size,
        sort_direction,
        items_view,
    }
}

fn default_turn_params(thread_id: ThreadId) -> ListTurnsParams {
    turn_params(
        thread_id,
        /*cursor*/ None,
        /*page_size*/ 10,
        SortDirection::Asc,
        StoredTurnItemsView::NotLoaded,
    )
}

async fn assert_unsupported(store: &PostgresThreadStore, thread_id: ThreadId) {
    assert!(matches!(
        store.list_turns(default_turn_params(thread_id)).await,
        Err(ThreadStoreError::Unsupported {
            operation: "list_turns"
        })
    ));
}

async fn assert_invalid(store: &PostgresThreadStore, params: ListTurnsParams) {
    assert!(matches!(
        store.list_turns(params).await,
        Err(ThreadStoreError::InvalidRequest { .. })
    ));
}

async fn damage_history_projections(
    fixture: &PostgresThreadStoreFixture,
    thread_id: ThreadId,
) -> Result<(), Box<dyn std::error::Error>> {
    let pool = sqlx::PgPool::connect(&fixture.database_url).await?;
    let schema = &fixture.schema;
    let mut transaction = pool.begin().await?;
    for table in ["thread_items", "thread_turns"] {
        sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM \"{schema}\".{table} WHERE thread_id = $1"
        )))
        .bind(thread_id.to_string())
        .execute(transaction.as_mut())
        .await?;
    }
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE \"{schema}\".threads SET history_projection_version = NULL WHERE thread_id = $1"
    )))
    .bind(thread_id.to_string())
    .execute(transaction.as_mut())
    .await?;
    transaction.commit().await?;
    pool.close().await;
    Ok(())
}

fn turn_ids(page: &crate::TurnPage) -> Vec<&str> {
    page.turns
        .iter()
        .map(|turn| turn.turn_id.as_str())
        .collect()
}

fn item_ids(turn: &StoredTurn) -> Vec<&str> {
    turn.items
        .iter()
        .map(|item| item.item_id.as_str())
        .collect()
}
