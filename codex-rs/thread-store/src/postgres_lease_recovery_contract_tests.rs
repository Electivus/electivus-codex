use std::error::Error;

use codex_protocol::ThreadId;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::RolloutItem;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;

use crate::AppendBatchId;
use crate::AppendThreadItemsBatch;
use crate::LoadThreadHistoryParams;
use crate::PostgresThreadStore;
use crate::ThreadStore;
use crate::postgres_contract_tests::PostgresThreadStoreFixture;
use crate::postgres_contract_tests::create_thread_params;

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_active_writer_recovers_expired_lease_without_takeover()
-> Result<(), Box<dyn Error>> {
    let fixture = PostgresThreadStoreFixture::new("writer_lease_recovery")?;
    fixture.migrate().await?;
    let pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::new();
    writer
        .create_thread(create_thread_params(thread_id))
        .await?;

    expire_writer_lease(&pool, &fixture.schema, thread_id).await?;
    writer.persist_thread(thread_id).await?;

    expire_writer_lease(&pool, &fixture.schema, thread_id).await?;
    writer
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            vec![RolloutItem::Compacted(CompactedItem {
                message: "history after idle lease expiry".to_string(),
                replacement_history: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
            })],
        ))
        .await?;

    expire_writer_lease(&pool, &fixture.schema, thread_id).await?;
    writer.shutdown_thread(thread_id).await?;
    assert_eq!(
        writer
            .load_history(LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await?
            .items
            .len(),
        2
    );

    pool.close().await;
    fixture.cleanup().await
}

async fn expire_writer_lease(
    pool: &sqlx::PgPool,
    schema: &str,
    thread_id: ThreadId,
) -> Result<(), sqlx::Error> {
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE \"{schema}\".threads \
         SET writer_lease_expires_at = CURRENT_TIMESTAMP - INTERVAL '1 second' \
         WHERE thread_id = $1"
    )))
    .bind(thread_id.to_string())
    .execute(pool)
    .await?;
    Ok(())
}
