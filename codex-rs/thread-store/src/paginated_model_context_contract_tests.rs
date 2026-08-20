use std::path::Path;

use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::WarningEvent;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;

use crate::AppendThreadItemsParams;
use crate::LoadThreadHistoryParams;
use crate::PostgresThreadStore;
use crate::ThreadStore;
use crate::postgres_contract_tests::PostgresThreadStoreFixture;
use crate::postgres_contract_tests::create_thread_params;

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_paginated_model_context_pages_oversized_presentation_suffix()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("paginated_model_context_pages")?;
    fixture.migrate().await?;
    let pool = fixture.connect_pool().await?;
    let store = PostgresThreadStore::new(pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f050")?;
    let cwd = Path::new("/paginated-model-context-pages");
    let mut params = create_thread_params(thread_id);
    params.history_mode = ThreadHistoryMode::Paginated;
    params.metadata.cwd = Some(cwd.to_path_buf());
    store.create_thread(params).await?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![RolloutItem::ResponseItem(
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "retained user message".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }
                .into(),
            )],
        })
        .await?;

    let rollback = RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
        num_turns: 1,
    }));
    append_repeated_item(
        &pool,
        &store.tables.history,
        thread_id,
        /*first_ordinal*/ 2,
        /*count*/ 1,
        &rollback,
    )
    .await?;
    let mut expected_items = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?
        .items;

    let large_presentation = completed_agent_message(
        thread_id,
        "presentation",
        "presentation-only".repeat(60_000),
    );
    append_repeated_item(
        &pool,
        &store.tables.history,
        thread_id,
        /*first_ordinal*/ 3,
        /*count*/ 18,
        &large_presentation,
    )
    .await?;
    let small_presentation =
        completed_agent_message(thread_id, "presentation", "presentation-only".to_string());
    append_repeated_item(
        &pool,
        &store.tables.history,
        thread_id,
        /*first_ordinal*/ 21,
        /*count*/ 43,
        &small_presentation,
    )
    .await?;

    let retained_page_end = warning("retained at page end");
    let retained_page_start = warning("retained at page start");
    let retained_after_gap = warning("retained after ordinal gap");
    for (ordinal, item) in [
        (64, &retained_page_end),
        (65, &retained_page_start),
        (97, &retained_after_gap),
    ] {
        append_repeated_item(
            &pool,
            &store.tables.history,
            thread_id,
            ordinal,
            /*count*/ 1,
            item,
        )
        .await?;
    }
    append_repeated_item(
        &pool,
        &store.tables.history,
        thread_id,
        /*first_ordinal*/ 66,
        /*count*/ 24,
        &small_presentation,
    )
    .await?;
    append_repeated_item(
        &pool,
        &store.tables.history,
        thread_id,
        /*first_ordinal*/ 98,
        /*count*/ 3,
        &small_presentation,
    )
    .await?;
    expected_items.extend([retained_page_end, retained_page_start, retained_after_gap]);

    let stored_bytes: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT SUM(octet_length(item::text))::bigint FROM {} WHERE thread_id = $1",
        store.tables.history
    )))
    .bind(thread_id.to_string())
    .fetch_one(&pool)
    .await?;
    assert!(stored_bytes > 16 * 1024 * 1024);

    let actual_items = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?
        .items;
    assert_eq!(
        serde_json::to_value(actual_items)?,
        serde_json::to_value(expected_items)?
    );
    let durable_presentation_items: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*)::bigint FROM {} \
         WHERE thread_id = $1 AND item #>> '{{payload,type}}' = 'item_completed'",
        store.tables.history
    )))
    .bind(thread_id.to_string())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_presentation_items, 88);

    pool.close().await;
    fixture.cleanup().await
}

fn completed_agent_message(thread_id: ThreadId, turn_id: &str, text: String) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: turn_id.to_string(),
        item: TurnItem::AgentMessage(AgentMessageItem {
            id: format!("agent-{turn_id}"),
            content: vec![AgentMessageContent::Text { text }],
            phase: None,
            memory_citation: None,
        }),
        started_at_ms: Some(0),
        completed_at_ms: 0,
    }))
}

fn warning(message: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::Warning(WarningEvent {
        message: message.to_string(),
    }))
}

async fn append_repeated_item(
    pool: &sqlx::PgPool,
    history_table: &str,
    thread_id: ThreadId,
    first_ordinal: i64,
    count: i64,
    item: &RolloutItem,
) -> Result<(), Box<dyn std::error::Error>> {
    let last_ordinal = first_ordinal
        .checked_add(count)
        .and_then(|ordinal| ordinal.checked_sub(1))
        .ok_or("history fixture ordinal overflow")?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO {history_table} (thread_id, ordinal, item, recorded_at) \
         SELECT $1, ordinal, $2, CURRENT_TIMESTAMP \
         FROM generate_series($3, $4) AS ordinal"
    )))
    .bind(thread_id.to_string())
    .bind(serde_json::to_value(item)?)
    .bind(first_ordinal)
    .bind(last_ordinal)
    .execute(pool)
    .await?;
    Ok(())
}
