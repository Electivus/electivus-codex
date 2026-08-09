#![allow(
    clippy::disallowed_methods,
    reason = "PostgreSQL tests connect only to PostgreSQL pools"
)]

use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadHistoryMode;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;

use crate::AppendBatchId;
use crate::AppendThreadItemsBatch;
use crate::ItemSortKey;
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
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let reader = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
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
            sort_key: ItemSortKey::CreatedAtOrdinal,
            after_updated_at_ordinal: None,
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
            sort_key: ItemSortKey::CreatedAtOrdinal,
            after_updated_at_ordinal: None,
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
            sort_key: ItemSortKey::CreatedAtOrdinal,
            after_updated_at_ordinal: None,
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
            sort_key: ItemSortKey::CreatedAtOrdinal,
            after_updated_at_ordinal: None,
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
            sort_key: ItemSortKey::CreatedAtOrdinal,
            after_updated_at_ordinal: None,
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
            sort_key: ItemSortKey::CreatedAtOrdinal,
            after_updated_at_ordinal: None,
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
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let reader = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
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
                sort_key: ItemSortKey::CreatedAtOrdinal,
                after_updated_at_ordinal: None,
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
                    sort_key: ItemSortKey::CreatedAtOrdinal,
                    after_updated_at_ordinal: None,
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
    let pool = fixture.connect_pool().await?;
    let store = PostgresThreadStore::new(pool.clone(), fixture.schema.clone());
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
            sort_key: ItemSortKey::CreatedAtOrdinal,
            after_updated_at_ordinal: None,
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

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_append_rebuilds_stale_prefix_before_projecting_suffix()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_projection_append_rebuild")?;
    fixture.migrate().await?;
    let pool = fixture.connect_pool().await?;
    let store = PostgresThreadStore::new(pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f014")?;
    let mut create_params = create_thread_params(thread_id);
    create_params.history_mode = ThreadHistoryMode::Paginated;
    store.create_thread(create_params).await?;
    let history = projected_item_history(thread_id);
    store
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            history[..2].to_vec(),
        ))
        .await?;
    delete_item_projection(&fixture, thread_id).await?;

    store
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            history[2..].to_vec(),
        ))
        .await?;

    assert_eq!(
        item_ids(&store.list_items(default_item_params(thread_id)).await?),
        vec!["item-1", "item-2", "item-3", "item-4"]
    );
    pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_failed_rebuild_preserves_projection_and_checkpoint()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_projection_rebuild_rollback")?;
    fixture.migrate().await?;
    let pool = fixture.connect_pool().await?;
    let store = PostgresThreadStore::new(pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f015")?;
    let mut create_params = create_thread_params(thread_id);
    create_params.history_mode = ThreadHistoryMode::Paginated;
    store.create_thread(create_params).await?;
    assert_eq!(
        projection_snapshot(&fixture, thread_id).await?,
        (Some(1), Vec::new())
    );
    store
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            projected_item_history(thread_id)[..1].to_vec(),
        ))
        .await?;
    assert_eq!(
        projection_snapshot(&fixture, thread_id).await?,
        (Some(2), vec!["item-1".to_string()])
    );
    invalidate_item_projection(&fixture, thread_id).await?;
    set_item_projection_writes(&fixture, ProjectionWrites::Reject).await?;
    let before = projection_snapshot(&fixture, thread_id).await?;

    assert!(matches!(
        store.list_items(default_item_params(thread_id)).await,
        Err(crate::ThreadStoreError::Internal { .. })
    ));
    assert_eq!(projection_snapshot(&fixture, thread_id).await?, before);

    set_item_projection_writes(&fixture, ProjectionWrites::Allow).await?;
    assert_eq!(
        item_ids(&store.list_items(default_item_params(thread_id)).await?),
        vec!["item-1"]
    );
    pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_confirmed_retry_does_not_depend_on_projection_rebuild()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_projection_retry_rebuild")?;
    fixture.migrate().await?;
    let pool = fixture.connect_pool().await?;
    let store = PostgresThreadStore::new(pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f016")?;
    let mut create_params = create_thread_params(thread_id);
    create_params.history_mode = ThreadHistoryMode::Paginated;
    store.create_thread(create_params).await?;
    let batch = AppendThreadItemsBatch::new(
        thread_id,
        AppendBatchId::new(),
        projected_item_history(thread_id)[..1].to_vec(),
    );
    let committed = store.append_batch(batch.clone()).await?;
    invalidate_item_projection(&fixture, thread_id).await?;
    set_item_projection_writes(&fixture, ProjectionWrites::Reject).await?;

    assert_eq!(store.append_batch(batch).await?, committed);
    assert_eq!(
        store
            .load_history(crate::LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await?
            .items
            .len(),
        2
    );

    set_item_projection_writes(&fixture, ProjectionWrites::Allow).await?;
    assert_eq!(
        item_ids(&store.list_items(default_item_params(thread_id)).await?),
        vec!["item-1"]
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
            started_at_ms: Some(0),
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

fn default_item_params(thread_id: ThreadId) -> ListItemsParams {
    ListItemsParams {
        thread_id,
        turn_id: None,
        include_archived: false,
        cursor: None,
        page_size: 10,
        sort_direction: SortDirection::Asc,
        sort_key: ItemSortKey::CreatedAtOrdinal,
        after_updated_at_ordinal: None,
    }
}

async fn delete_item_projection(
    fixture: &PostgresThreadStoreFixture,
    thread_id: ThreadId,
) -> Result<(), Box<dyn std::error::Error>> {
    execute_item_projection_sql(
        fixture,
        thread_id,
        "DELETE FROM {items} WHERE thread_id = $1",
    )
    .await
}

async fn invalidate_item_projection(
    fixture: &PostgresThreadStoreFixture,
    thread_id: ThreadId,
) -> Result<(), Box<dyn std::error::Error>> {
    execute_item_projection_sql(
        fixture,
        thread_id,
        "UPDATE {items} SET item = item WHERE thread_id = $1",
    )
    .await
}

async fn execute_item_projection_sql(
    fixture: &PostgresThreadStoreFixture,
    thread_id: ThreadId,
    sql: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let pool = sqlx::PgPool::connect(&fixture.database_url).await?;
    let items = format!("\"{}\".thread_items", fixture.schema);
    let threads = format!("\"{}\".threads", fixture.schema);
    let mut transaction = pool.begin().await?;
    sqlx::query(AssertSqlSafe(sql.replace("{items}", items.as_str())))
        .bind(thread_id.to_string())
        .execute(transaction.as_mut())
        .await?;
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {threads} SET history_projection_version = NULL WHERE thread_id = $1"
    )))
    .bind(thread_id.to_string())
    .execute(transaction.as_mut())
    .await?;
    transaction.commit().await?;
    pool.close().await;
    Ok(())
}

async fn projection_snapshot(
    fixture: &PostgresThreadStoreFixture,
    thread_id: ThreadId,
) -> Result<(Option<i64>, Vec<String>), Box<dyn std::error::Error>> {
    let pool = sqlx::PgPool::connect(&fixture.database_url).await?;
    let schema = &fixture.schema;
    let version = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT history_projection_version FROM \"{schema}\".threads WHERE thread_id = $1"
    )))
    .bind(thread_id.to_string())
    .fetch_one(&pool)
    .await?;
    let items = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT item_id FROM \"{schema}\".thread_items WHERE thread_id = $1 ORDER BY rollout_ordinal"
    )))
    .bind(thread_id.to_string())
    .fetch_all(&pool)
    .await?;
    pool.close().await;
    Ok((version, items))
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
