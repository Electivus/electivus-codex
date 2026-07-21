use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_state::PostgresRuntimeStatePool;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;

use crate::AppendBatchId;
use crate::AppendThreadItemsBatch;
use crate::ListItemsParams;
use crate::PostgresThreadStore;
use crate::SortDirection;
use crate::ThreadStore;
use crate::postgres_contract_tests::PostgresThreadStoreFixture;
use crate::postgres_contract_tests::create_thread_params;

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_canonical_appends_project_items_for_bidirectional_pagination()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_item_projection")?;
    fixture.migrate().await?;
    let first_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let second_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let writer = PostgresThreadStore::new(&first_pool);
    let reader = PostgresThreadStore::new(&second_pool);
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f011")?;
    let mut create_params = create_thread_params(thread_id);
    create_params.history_mode = ThreadHistoryMode::Paginated;
    writer.create_thread(create_params).await?;
    let batch_id = AppendBatchId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331ba05")?;
    let batch = AppendThreadItemsBatch::new(thread_id, batch_id, projected_item_history(thread_id));
    let original_commit = writer.append_batch(batch.clone()).await?;

    let first_page = reader
        .list_items(ListItemsParams {
            thread_id,
            turn_id: None,
            include_archived: false,
            cursor: None,
            page_size: 2,
            sort_direction: SortDirection::Asc,
        })
        .await?;
    assert_eq!(item_ids(&first_page), vec!["item-1", "item-2"]);
    let second_page = reader
        .list_items(ListItemsParams {
            thread_id,
            turn_id: None,
            include_archived: false,
            cursor: first_page.next_cursor,
            page_size: 2,
            sort_direction: SortDirection::Asc,
        })
        .await?;
    assert_eq!(item_ids(&second_page), vec!["item-3", "item-4"]);
    let backwards_page = reader
        .list_items(ListItemsParams {
            thread_id,
            turn_id: None,
            include_archived: false,
            cursor: second_page.backwards_cursor,
            page_size: 2,
            sort_direction: SortDirection::Desc,
        })
        .await?;
    assert_eq!(item_ids(&backwards_page), vec!["item-3", "item-2"]);
    let turn_page = reader
        .list_items(ListItemsParams {
            thread_id,
            turn_id: Some("turn-2".to_string()),
            include_archived: false,
            cursor: None,
            page_size: 2,
            sort_direction: SortDirection::Desc,
        })
        .await?;
    assert_eq!(item_ids(&turn_page), vec!["item-4", "item-3"]);

    assert_eq!(writer.append_batch(batch).await?, original_commit);
    let after_retry = reader
        .list_items(ListItemsParams {
            thread_id,
            turn_id: None,
            include_archived: false,
            cursor: None,
            page_size: 10,
            sort_direction: SortDirection::Asc,
        })
        .await?;
    assert_eq!(
        item_ids(&after_retry),
        vec!["item-1", "item-2", "item-3", "item-4"]
    );
    assert_eq!(
        after_retry
            .items
            .iter()
            .map(|item| serde_json::from_slice::<serde_json::Value>(&item.item_json))
            .collect::<Result<Vec<_>, _>>()?,
        ["item-1", "item-2", "item-3", "item-4"]
            .map(|item_id| {
                serde_json::json!({
                    "type": "userMessage",
                    "id": item_id,
                    "clientId": null,
                    "content": [],
                })
            })
            .to_vec()
    );
    assert!(after_retry.items.iter().all(|item| item.created_at_ms > 0));
    let missing_error = reader
        .list_items(ListItemsParams {
            thread_id: ThreadId::default(),
            turn_id: None,
            include_archived: false,
            cursor: None,
            page_size: 10,
            sort_direction: SortDirection::Asc,
        })
        .await
        .expect_err("an unindexed thread must preserve the local list-items contract");
    assert!(matches!(
        missing_error,
        crate::ThreadStoreError::Unsupported {
            operation: "list_items"
        }
    ));

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_projection_failure_rolls_back_canonical_append()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_item_projection_atomicity")?;
    fixture.migrate().await?;
    let first_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let second_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let writer = PostgresThreadStore::new(&first_pool);
    let reader = PostgresThreadStore::new(&second_pool);
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f012")?;
    let mut create_params = create_thread_params(thread_id);
    create_params.history_mode = ThreadHistoryMode::Paginated;
    writer.create_thread(create_params).await?;
    let batch = AppendThreadItemsBatch::new(
        thread_id,
        AppendBatchId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331ba06")?,
        projected_item_history(thread_id)
            .into_iter()
            .take(1)
            .collect(),
    );
    set_item_projection_writes(&fixture, ProjectionWrites::Reject).await?;

    let append_error = writer
        .append_batch(batch.clone())
        .await
        .expect_err("projection failure must reject the whole append");
    assert!(matches!(
        append_error,
        crate::ThreadStoreError::Internal { .. }
    ));
    set_item_projection_writes(&fixture, ProjectionWrites::Allow).await?;
    assert_eq!(
        reader
            .load_history(crate::LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await?
            .items
            .len(),
        1
    );
    assert_eq!(
        reader
            .list_items(ListItemsParams {
                thread_id,
                turn_id: None,
                include_archived: false,
                cursor: None,
                page_size: 10,
                sort_direction: SortDirection::Asc,
            })
            .await?
            .items,
        Vec::new()
    );

    writer.append_batch(batch).await?;
    assert_eq!(
        item_ids(
            &reader
                .list_items(ListItemsParams {
                    thread_id,
                    turn_id: None,
                    include_archived: false,
                    cursor: None,
                    page_size: 10,
                    sort_direction: SortDirection::Asc,
                })
                .await?
        ),
        vec!["item-1"]
    );

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_item_projection_excludes_inherited_subagent_prefix()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_item_projection_subagent")?;
    fixture.migrate().await?;
    let pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let store = PostgresThreadStore::new(&pool);
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f013")?;
    let mut create_params = create_thread_params(thread_id);
    create_params.history_mode = ThreadHistoryMode::Paginated;
    create_params.subagent_history_start_ordinal = Some(3);
    store.create_thread(create_params).await?;

    store
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331ba07")?,
            projected_item_history(thread_id),
        ))
        .await?;

    let page = store
        .list_items(ListItemsParams {
            thread_id,
            turn_id: None,
            include_archived: false,
            cursor: None,
            page_size: 10,
            sort_direction: SortDirection::Asc,
        })
        .await?;
    assert_eq!(item_ids(&page), vec!["item-3", "item-4"]);
    assert_eq!(
        store
            .load_history(crate::LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await?
            .items
            .len(),
        5
    );

    pool.close().await;
    fixture.cleanup().await
}

fn projected_item_history(thread_id: ThreadId) -> Vec<RolloutItem> {
    [
        ("turn-1", "item-1"),
        ("turn-1", "item-2"),
        ("turn-2", "item-3"),
        ("turn-2", "item-4"),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (turn_id, item_id))| {
        RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id,
            turn_id: turn_id.to_string(),
            item: TurnItem::UserMessage(UserMessageItem {
                id: item_id.to_string(),
                client_id: None,
                content: Vec::new(),
            }),
            completed_at_ms: i64::try_from(index + 1).expect("small fixture index") * 1_000,
        }))
    })
    .collect()
}

fn item_ids(page: &crate::ItemPage) -> Vec<&str> {
    page.items
        .iter()
        .map(|item| item.item_id.as_str())
        .collect()
}

enum ProjectionWrites {
    Reject,
    Allow,
}

async fn set_item_projection_writes(
    fixture: &PostgresThreadStoreFixture,
    writes: ProjectionWrites,
) -> Result<(), Box<dyn std::error::Error>> {
    let schema = &fixture.schema;
    let statement = match writes {
        ProjectionWrites::Reject => format!(
            "ALTER TABLE \"{schema}\".thread_items \
             ADD CONSTRAINT reject_item_projection_writes CHECK (false) NOT VALID"
        ),
        ProjectionWrites::Allow => format!(
            "ALTER TABLE \"{schema}\".thread_items \
             DROP CONSTRAINT reject_item_projection_writes"
        ),
    };
    let pool = sqlx::PgPool::connect(&fixture.database_url).await?;
    sqlx::query(AssertSqlSafe(statement)).execute(&pool).await?;
    pool.close().await;
    Ok(())
}
