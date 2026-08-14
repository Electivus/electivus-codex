use std::path::Path;

use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::AgentReasoningEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutItem;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;
use tempfile::TempDir;

use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::LocalThreadStore;
use crate::LocalThreadStoreConfig;
use crate::PostgresThreadStore;
use crate::SearchThreadsParams;
use crate::SortDirection;
use crate::ThreadSortKey;
use crate::ThreadStore;
use crate::list_threads_contract_tests::ListedThread;
use crate::list_threads_contract_tests::create_listed_thread;
use crate::postgres_contract_tests::PostgresThreadStoreFixture;
use crate::postgres_contract_tests::create_thread_params;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[tokio::test]
async fn local_search_threads_matches_public_store_contract() -> TestResult {
    let home = TempDir::new()?;
    let config = LocalThreadStoreConfig {
        codex_home: home.path().to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        default_model_provider_id: "search-contract-provider".to_string(),
    };
    let runtime = codex_state::StateRuntime::init_sqlite(
        home.path().to_path_buf(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await?;
    let store = LocalThreadStore::new(config, Some(runtime));

    assert_basic_search_contract(&store, home.path()).await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_search_matches_across_replicas_and_is_atomic() -> TestResult {
    let fixture = PostgresThreadStoreFixture::new("search_projection")?;
    fixture.migrate().await?;
    let writer_pool = fixture.connect_pool().await?;
    let reader_pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(writer_pool.clone(), fixture.schema.clone());
    let reader = PostgresThreadStore::new(reader_pool.clone(), fixture.schema.clone());
    assert_basic_search_contract(&writer, Path::new("/search-contract")).await?;
    let low = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f410")?;
    let high = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f411")?;
    for thread_id in [low, high] {
        create_searchable_thread(
            &writer,
            thread_id,
            Path::new("/search"),
            SessionSource::Cli,
            ThreadHistoryMode::Legacy,
            "2040-01-01T00:00:00Z",
            vec![visible_user("needle[1]")],
        )
        .await?;
    }
    let mut cursors = Vec::new();
    for sort_key in [
        ThreadSortKey::CreatedAt,
        ThreadSortKey::UpdatedAt,
        ThreadSortKey::RecencyAt,
    ] {
        let mut first_query = search_params(sort_key, SortDirection::Desc);
        first_query.page_size = 1;
        let first = reader.search_threads(first_query).await?;
        assert_eq!(ids(&first), vec![high]);
        cursors.push((
            sort_key,
            first.next_cursor.expect("first page should have a cursor"),
        ));
    }
    let newer = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f412")?;
    create_searchable_thread(
        &writer,
        newer,
        Path::new("/search"),
        SessionSource::Cli,
        ThreadHistoryMode::Legacy,
        "2040-01-02T00:00:00Z",
        vec![visible_user("needle[1]")],
    )
    .await?;
    for (sort_key, cursor) in cursors {
        let mut query = search_params(sort_key, SortDirection::Desc);
        query.page_size = 1;
        query.cursor = Some(cursor);
        assert_search_ids(&reader, query, &[low]).await?;
    }

    let paginated = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f420")?;
    let item = RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: paginated,
        turn_id: "turn-search".to_string(),
        item: TurnItem::UserMessage(UserMessageItem {
            id: "item-search".to_string(),
            client_id: None,
            content: vec![UserInput::Text {
                text: "paginated needle[1]".to_string(),
                text_elements: vec![],
            }],
        }),
        started_at_ms: Some(0),
        completed_at_ms: 1,
    }));
    create_searchable_thread(
        &writer,
        paginated,
        Path::new("/search"),
        SessionSource::Exec,
        ThreadHistoryMode::Paginated,
        "2030-01-03T00:00:00Z",
        vec![hidden_tool("needle[1]"), item],
    )
    .await?;
    assert_eq!(delete_search_projection(&writer, paginated).await?, 1);
    let page = reader
        .search_threads(search_params(ThreadSortKey::CreatedAt, SortDirection::Desc))
        .await?;
    assert!(ids(&page).contains(&paginated));

    let atomic = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f421")?;
    let mut params = create_thread_params(atomic);
    params.source = SessionSource::Cli;
    writer.create_thread(params).await?;
    let append = AppendThreadItemsParams {
        thread_id: atomic,
        items: vec![visible_user("atomic needle[1]")],
    };
    set_projection_writes(&writer, ProjectionWrites::Reject).await?;
    assert!(writer.append_items(append.clone()).await.is_err());
    set_projection_writes(&writer, ProjectionWrites::Allow).await?;
    let history = reader
        .load_history(crate::LoadThreadHistoryParams {
            thread_id: atomic,
            include_archived: false,
        })
        .await?;
    assert_eq!(history.items.len(), 1);
    assert_eq!(delete_search_projection(&writer, atomic).await?, 0);
    writer.append_items(append).await?;
    let page = reader
        .search_threads(search_params(ThreadSortKey::CreatedAt, SortDirection::Desc))
        .await?;
    assert!(ids(&page).contains(&atomic));

    writer_pool.close().await;
    reader_pool.close().await;
    fixture.cleanup().await
}

async fn assert_basic_search_contract(store: &dyn ThreadStore, cwd: &Path) -> TestResult {
    let first = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f401")?;
    let second = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f402")?;
    let hidden = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f403")?;
    let archived = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f404")?;
    for (thread_id, source, timestamp, items) in [
        (
            first,
            SessionSource::Cli,
            "2030-01-01T00:00:00Z",
            vec![
                visible_user("first NeEdLe[1] match"),
                visible_agent("later needle[1] match"),
            ],
        ),
        (
            second,
            SessionSource::Exec,
            "2030-01-02T00:00:00Z",
            vec![visible_user("second needle[1] match")],
        ),
        (
            hidden,
            SessionSource::Cli,
            "2030-01-03T00:00:00Z",
            vec![
                hidden_tool("needle[1]"),
                visible_user("ordinary visible text"),
            ],
        ),
        (
            archived,
            SessionSource::Cli,
            "2030-01-04T00:00:00Z",
            vec![visible_user("archived needle[1]")],
        ),
    ] {
        create_searchable_thread(
            store,
            thread_id,
            cwd,
            source,
            ThreadHistoryMode::Legacy,
            timestamp,
            items,
        )
        .await?;
    }
    store
        .archive_thread(ArchiveThreadParams {
            thread_id: archived,
        })
        .await?;

    let descending = store
        .search_threads(search_params(ThreadSortKey::CreatedAt, SortDirection::Desc))
        .await?;
    assert_eq!(ids(&descending), vec![second, first]);
    assert_eq!(descending.items[1].snippet, "first NeEdLe[1] match");
    for sort_key in [
        ThreadSortKey::CreatedAt,
        ThreadSortKey::UpdatedAt,
        ThreadSortKey::RecencyAt,
    ] {
        assert_search_ids(
            store,
            search_params(sort_key, SortDirection::Asc),
            &[first, second],
        )
        .await?;
    }
    let mut query = search_params(ThreadSortKey::CreatedAt, SortDirection::Desc);
    query.allowed_sources = vec![SessionSource::Cli];
    assert_search_ids(store, query, &[first]).await?;
    let mut query = search_params(ThreadSortKey::CreatedAt, SortDirection::Desc);
    query.archived = true;
    assert_search_ids(store, query, &[archived]).await?;
    let mut query = search_params(ThreadSortKey::CreatedAt, SortDirection::Asc);
    query.page_size = 1;
    let first_page = store.search_threads(query).await?;
    assert_eq!(ids(&first_page), vec![first]);
    let mut query = search_params(ThreadSortKey::CreatedAt, SortDirection::Asc);
    query.page_size = 1;
    query.cursor = first_page.next_cursor;
    assert_search_ids(store, query, &[second]).await?;
    Ok(())
}

async fn create_searchable_thread(
    store: &dyn ThreadStore,
    thread_id: ThreadId,
    cwd: &Path,
    source: SessionSource,
    history_mode: ThreadHistoryMode,
    timestamp: &str,
    items: Vec<RolloutItem>,
) -> TestResult {
    create_listed_thread(
        store,
        ListedThread {
            thread_id,
            cwd,
            timestamp,
            source,
            model_provider: "search-contract-provider",
            parent_thread_id: None,
            preview: "searchable thread",
            name: None,
            history_mode,
            items,
        },
    )
    .await
}

fn search_params(sort_key: ThreadSortKey, sort_direction: SortDirection) -> SearchThreadsParams {
    SearchThreadsParams {
        page_size: 10,
        cursor: None,
        sort_key,
        sort_direction,
        allowed_sources: Vec::new(),
        archived: false,
        search_term: "needle[1]".to_string(),
    }
}

fn ids(page: &crate::ThreadSearchPage) -> Vec<ThreadId> {
    page.items
        .iter()
        .map(|item| item.thread.thread_id)
        .collect()
}

async fn assert_search_ids(
    store: &dyn ThreadStore,
    params: SearchThreadsParams,
    expected: &[ThreadId],
) -> TestResult {
    assert_eq!(ids(&store.search_threads(params).await?), expected);
    Ok(())
}

fn visible_user(message: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        message: message.to_string(),
        ..Default::default()
    }))
}

fn visible_agent(message: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
        message: message.to_string(),
        phase: None,
        memory_citation: None,
    }))
}

fn hidden_tool(text: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::AgentReasoning(AgentReasoningEvent {
        text: text.to_string(),
    }))
}

enum ProjectionWrites {
    Reject,
    Allow,
}

async fn set_projection_writes(
    store: &PostgresThreadStore,
    writes: ProjectionWrites,
) -> TestResult {
    let statement = match writes {
        ProjectionWrites::Reject => format!(
            "ALTER TABLE {} ADD CONSTRAINT reject_search_projection_writes CHECK (false) NOT VALID",
            store.tables.search_content
        ),
        ProjectionWrites::Allow => format!(
            "ALTER TABLE {} DROP CONSTRAINT reject_search_projection_writes",
            store.tables.search_content
        ),
    };
    sqlx::query(AssertSqlSafe(statement))
        .execute(&store.pool)
        .await?;
    Ok(())
}

async fn delete_search_projection(
    store: &PostgresThreadStore,
    thread_id: ThreadId,
) -> TestResult<u64> {
    let mut transaction = store.pool.begin().await?;
    let result = sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {} WHERE thread_id = $1",
        store.tables.search_content
    )))
    .bind(thread_id.to_string())
    .execute(transaction.as_mut())
    .await?;
    if result.rows_affected() > 0 {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET history_projection_version = NULL WHERE thread_id = $1",
            store.tables.threads
        )))
        .bind(thread_id.to_string())
        .execute(transaction.as_mut())
        .await?;
    }
    transaction.commit().await?;
    Ok(result.rows_affected())
}
