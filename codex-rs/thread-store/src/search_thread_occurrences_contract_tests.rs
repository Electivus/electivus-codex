use std::path::Path;

use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::items::ReasoningItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutItem;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;
use tempfile::TempDir;

use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::ListTurnsParams;
use crate::LocalThreadStore;
use crate::LocalThreadStoreConfig;
use crate::PersistContext;
use crate::PostgresThreadStore;
use crate::SearchThreadOccurrencesParams;
use crate::SortDirection;
use crate::StoredTurnItemsView;
use crate::ThreadOccurrenceSearchPage;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::postgres_contract_tests::PostgresThreadStoreFixture;
use crate::postgres_contract_tests::create_thread_params;
use crate::postgres_turn_projection_contract_tests::agent_item;
use crate::postgres_turn_projection_contract_tests::completed_item;
use crate::postgres_turn_projection_contract_tests::turn_complete;
use crate::postgres_turn_projection_contract_tests::turn_started;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[tokio::test]
async fn local_occurrence_search_matches_public_store_contract() -> TestResult {
    let home = TempDir::new()?;
    let config = LocalThreadStoreConfig {
        codex_home: home.path().to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        default_model_provider_id: "occurrence-search-contract".to_string(),
    };
    let runtime = codex_state::StateRuntime::init_sqlite(
        home.path().to_path_buf(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    for (thread_id, history_mode) in [
        (thread_id(/*suffix*/ 41)?, ThreadHistoryMode::Paginated),
        (thread_id(/*suffix*/ 42)?, ThreadHistoryMode::Paginated),
        (thread_id(/*suffix*/ 43)?, ThreadHistoryMode::Legacy),
    ] {
        let mut builder = codex_state::ThreadMetadataBuilder::new(
            thread_id,
            home.path().join(format!("{thread_id}.jsonl")),
            Utc::now(),
            SessionSource::Exec,
        );
        builder.history_mode = history_mode;
        runtime
            .upsert_thread(&builder.build(config.default_model_provider_id.as_str()))
            .await?;
    }
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await?;
    let store = LocalThreadStore::new(config, Some(runtime));
    assert_contract(&store, home.path()).await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_occurrence_search_matches_across_replicas_and_repairs() -> TestResult {
    let fixture = PostgresThreadStoreFixture::new("occurrence_search")?;
    fixture.migrate().await?;
    let writer_pool = fixture.connect_pool().await?;
    let reader_pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(writer_pool.clone(), fixture.schema.clone());
    let reader = PostgresThreadStore::new(reader_pool.clone(), fixture.schema.clone());
    let cwd = Path::new("/occurrence-search");
    assert_contract(&writer, cwd).await?;

    let thread_id = thread_id(/*suffix*/ 44)?;
    create_thread(
        &writer,
        thread_id,
        cwd,
        ThreadHistoryMode::Paginated,
        history(thread_id),
    )
    .await?;
    assert_eq!(
        item_ids(
            &search(
                &reader, thread_id, "needle", /*cursor*/ None, /*page_size*/ 10
            )
            .await?
        ),
        vec!["user-1", "user-1", "user-1", "user-1", "steer-1", "final-1"]
    );
    damage_projections(&writer, thread_id).await?;
    assert_eq!(
        item_ids(
            &search(
                &reader, thread_id, "école", /*cursor*/ None, /*page_size*/ 10
            )
            .await?
        ),
        vec!["steer-1"]
    );

    writer_pool.close().await;
    reader_pool.close().await;
    fixture.cleanup().await
}

async fn assert_contract(store: &dyn ThreadStore, cwd: &Path) -> TestResult {
    assert!(store.supports_paginated_history_lists());
    let (primary, other, legacy) = (
        thread_id(/*suffix*/ 41)?,
        thread_id(/*suffix*/ 42)?,
        thread_id(/*suffix*/ 43)?,
    );
    create_thread(
        store,
        primary,
        cwd,
        ThreadHistoryMode::Paginated,
        history(primary),
    )
    .await?;
    create_thread(store, other, cwd, ThreadHistoryMode::Paginated, Vec::new()).await?;
    create_thread(store, legacy, cwd, ThreadHistoryMode::Legacy, Vec::new()).await?;

    let first = search(
        store, primary, "needle", /*cursor*/ None, /*page_size*/ 3,
    )
    .await?;
    let turn_cursor = first.items[0].turn_cursor.clone();
    assert_eq!(
        occurrences(&first),
        vec![
            ("user-1", "😀 NEEDLE needle needle needle", 3, 9),
            ("user-1", "😀 NEEDLE needle needle needle", 10, 16),
            ("user-1", "😀 NEEDLE needle needle needle", 17, 23),
        ]
    );
    let next = first.next_cursor.ok_or("first page must continue")?;
    let second = search(
        store,
        primary,
        "needle",
        Some(next.clone()),
        /*page_size*/ 3,
    )
    .await?;
    assert_eq!(
        occurrences(&second),
        vec![
            ("user-1", "😀 NEEDLE needle needle needle", 24, 30),
            ("steer-1", "steer needle ÉCOLE", 6, 12),
            ("final-1", "😀 Final needle", 9, 15),
        ]
    );
    assert!(second.next_cursor.is_none());
    assert!(
        first
            .items
            .iter()
            .chain(&second.items)
            .all(|item| { item.turn_id == "turn-1" && item.turn_cursor == turn_cursor })
    );
    assert_eq!(
        item_ids(
            &search(
                store, primary, "école", /*cursor*/ None, /*page_size*/ 10
            )
            .await?
        ),
        vec!["steer-1"]
    );

    let turns = store
        .list_turns(ListTurnsParams {
            thread_id: primary,
            include_archived: true,
            cursor: Some(turn_cursor),
            page_size: 1,
            sort_direction: SortDirection::Asc,
            items_view: StoredTurnItemsView::Summary,
        })
        .await?;
    assert_eq!(turns.turns[0].turn_id, "turn-1");
    assert_eq!(
        turns.turns[0]
            .items
            .iter()
            .map(|item| item.item_id.as_str())
            .collect::<Vec<_>>(),
        vec!["user-1", "final-1"]
    );

    for params in [
        search_params(
            primary,
            "needle",
            Some(format!("{next}!")),
            /*page_size*/ 3,
        ),
        search_params(other, "needle", Some(next.clone()), /*page_size*/ 3),
        search_params(primary, "Needle", Some(next), /*page_size*/ 3),
        search_params(primary, " ", /*cursor*/ None, /*page_size*/ 3),
        search_params(
            primary, "needle", /*cursor*/ None, /*page_size*/ 0,
        ),
    ] {
        assert!(matches!(
            store.search_thread_occurrences(params).await,
            Err(ThreadStoreError::InvalidRequest { .. })
        ));
    }
    assert!(matches!(
        search(
            store, legacy, "needle", /*cursor*/ None, /*page_size*/ 3
        )
        .await,
        Err(ThreadStoreError::Unsupported {
            operation: "thread/searchOccurrences"
        })
    ));
    store
        .archive_thread(ArchiveThreadParams { thread_id: primary })
        .await?;
    assert_eq!(
        item_ids(
            &search(
                store, primary, "école", /*cursor*/ None, /*page_size*/ 10
            )
            .await?
        ),
        vec!["steer-1"]
    );
    Ok(())
}

async fn create_thread(
    store: &dyn ThreadStore,
    thread_id: ThreadId,
    cwd: &Path,
    history_mode: ThreadHistoryMode,
    items: Vec<RolloutItem>,
) -> TestResult {
    let mut params = create_thread_params(thread_id);
    params.history_mode = history_mode;
    params.metadata.cwd = Some(cwd.to_path_buf());
    store.create_thread(params).await?;
    store
        .persist_thread(thread_id, PersistContext::Standard)
        .await?;
    if !items.is_empty() {
        store
            .append_items(AppendThreadItemsParams { thread_id, items })
            .await?;
    }
    store.shutdown_thread(thread_id).await?;
    Ok(())
}

fn history(thread_id: ThreadId) -> Vec<RolloutItem> {
    vec![
        turn_started("turn-1", /*started_at*/ 1),
        completed_item(
            thread_id,
            "turn-1",
            user_item("user-1", &["😀 NEE", "DLE needle needle needle"]),
        ),
        completed_item(
            thread_id,
            "turn-1",
            user_item("steer-1", &["steer needle ÉCOLE"]),
        ),
        completed_item(
            thread_id,
            "turn-1",
            TurnItem::Reasoning(ReasoningItem {
                id: "reasoning-1".to_string(),
                summary_text: vec!["hidden needle".to_string()],
                raw_content: Vec::new(),
            }),
        ),
        completed_item(
            thread_id,
            "turn-1",
            agent_item(
                "commentary-1",
                "commentary needle",
                Some(MessagePhase::Commentary),
            ),
        ),
        completed_item(
            thread_id,
            "turn-1",
            agent_item("draft-1", "draft needle", /*phase*/ None),
        ),
        completed_item(
            thread_id,
            "turn-1",
            agent_item(
                "final-1",
                "😀 **Final**  \nneedle",
                Some(MessagePhase::FinalAnswer),
            ),
        ),
        turn_complete(
            "turn-1", /*started_at*/ 1, /*completed_at*/ 2, /*error*/ None,
        ),
    ]
}

fn user_item(item_id: &str, texts: &[&str]) -> TurnItem {
    TurnItem::UserMessage(UserMessageItem {
        id: item_id.to_string(),
        client_id: None,
        content: texts
            .iter()
            .map(|text| UserInput::Text {
                text: (*text).to_string(),
                text_elements: Vec::new(),
            })
            .collect(),
    })
}

fn thread_id(suffix: u8) -> TestResult<ThreadId> {
    Ok(ThreadId::from_string(&format!(
        "0198c4cf-8587-7d32-8d1c-2c14d331f0{suffix}"
    ))?)
}

fn search_params(
    thread_id: ThreadId,
    term: &str,
    cursor: Option<String>,
    page_size: usize,
) -> SearchThreadOccurrencesParams {
    SearchThreadOccurrencesParams {
        thread_id,
        search_term: term.to_string(),
        cursor,
        page_size,
    }
}

async fn search(
    store: &dyn ThreadStore,
    thread_id: ThreadId,
    term: &str,
    cursor: Option<String>,
    page_size: usize,
) -> ThreadStoreResult<ThreadOccurrenceSearchPage> {
    store
        .search_thread_occurrences(search_params(thread_id, term, cursor, page_size))
        .await
}

fn occurrences(page: &ThreadOccurrenceSearchPage) -> Vec<(&str, &str, u32, u32)> {
    page.items
        .iter()
        .map(|item| {
            (
                item.item_id.as_str(),
                item.snippet.as_str(),
                item.snippet_match_range.start,
                item.snippet_match_range.end,
            )
        })
        .collect()
}

fn item_ids(page: &ThreadOccurrenceSearchPage) -> Vec<&str> {
    page.items
        .iter()
        .map(|item| item.item_id.as_str())
        .collect()
}

async fn damage_projections(store: &PostgresThreadStore, thread_id: ThreadId) -> TestResult {
    let mut transaction = store.pool.begin().await?;
    for table in [&store.tables.items, &store.tables.turns] {
        sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE thread_id = $1"
        )))
        .bind(thread_id.to_string())
        .execute(transaction.as_mut())
        .await?;
    }
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET history_projection_version = NULL WHERE thread_id = $1",
        store.tables.threads
    )))
    .bind(thread_id.to_string())
    .execute(transaction.as_mut())
    .await?;
    transaction.commit().await?;
    Ok(())
}
