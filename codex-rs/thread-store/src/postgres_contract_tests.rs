use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::UserMessageEvent;
use codex_state::PostgresNamespaceAction;
use codex_state::PostgresNamespaceConfig;
use codex_state::PostgresPoolConfig;
use codex_state::PostgresRuntimeStatePool;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;

use crate::AppendBatchId;
use crate::AppendThreadItemsBatch;
use crate::CreateThreadParams;
use crate::PostgresThreadStore;
use crate::ReadThreadParams;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;

const TEST_DATABASE_URL_ENV: &str = "CODEX_TEST_POSTGRES_URL";
static NEXT_SCHEMA_ID: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_flush_and_shutdown_preserve_durable_history()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_lifecycle")?;
    fixture.migrate().await?;
    let first_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let second_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let writer = PostgresThreadStore::new(&first_pool);
    let reader = PostgresThreadStore::new(&second_pool);
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f004")?;
    writer
        .create_thread(create_thread_params(thread_id))
        .await?;
    writer
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331ba03")?,
            append_items(),
        ))
        .await?;

    writer.persist_thread(thread_id).await?;
    writer.flush_thread(thread_id).await?;
    let durable = reader
        .load_history(crate::LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    writer.shutdown_thread(thread_id).await?;
    let after_shutdown = writer
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            append_items(),
        ))
        .await
        .expect_err("shutdown must end local writer ownership");
    assert!(matches!(
        after_shutdown,
        crate::ThreadStoreError::ThreadNotFound { .. }
    ));
    assert_eq!(
        serde_json::to_value(
            reader
                .load_history(crate::LoadThreadHistoryParams {
                    thread_id,
                    include_archived: false,
                })
                .await?
        )?,
        serde_json::to_value(durable)?
    );

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_append_batch_retries_are_idempotent()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_append_retry")?;
    fixture.migrate().await?;
    let pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let store = PostgresThreadStore::new(&pool);
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f003")?;
    store.create_thread(create_thread_params(thread_id)).await?;
    let batch_id = AppendBatchId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331ba02")?;
    let batch = AppendThreadItemsBatch::new(thread_id, batch_id, append_items());

    let first = store.append_batch(batch.clone()).await?;
    let retry = store.append_batch(batch.clone()).await?;
    assert_eq!(retry, first);

    let mismatch = store
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            batch_id,
            vec![RolloutItem::Compacted(CompactedItem {
                message: "different content".to_string(),
                replacement_history: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
            })],
        ))
        .await
        .expect_err("reusing a batch id with different content must conflict");
    assert!(matches!(mismatch, crate::ThreadStoreError::Conflict { .. }));
    let history = store
        .load_history(crate::LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    assert_eq!(history.items.len(), 4);

    pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_append_batch_preserves_full_history_and_projection()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_append")?;
    fixture.migrate().await?;
    let first_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let second_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let writer = PostgresThreadStore::new(&first_pool);
    let reader = PostgresThreadStore::new(&second_pool);
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f002")?;
    writer
        .create_thread(create_thread_params(thread_id))
        .await?;
    let initial_history = reader
        .load_history(crate::LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    let items = append_items();

    let committed = writer
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331ba01")?,
            items.clone(),
        ))
        .await?;

    assert_eq!(committed.first_ordinal, 1);
    assert_eq!(committed.persisted_item_count, 3);
    assert_eq!(committed.committed_stream_version, 4);
    let thread = reader
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived: false,
            include_history: true,
        })
        .await?;
    assert_eq!(thread.preview, "full fidelity user message");
    assert_eq!(
        thread.first_user_message.as_deref(),
        Some("full fidelity user message")
    );
    let mut expected_history = initial_history.items;
    expected_history.extend(items);
    assert_eq!(
        serde_json::to_value(thread.history.ok_or("history must be loaded")?.items)?,
        serde_json::to_value(expected_history)?
    );

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_create_is_readable_across_replicas_without_rollout()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_create")?;
    fixture.migrate().await?;
    let first_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let second_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let writer = PostgresThreadStore::new(&first_pool);
    let reader = PostgresThreadStore::new(&second_pool);
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f001")?;

    writer
        .create_thread(create_thread_params(thread_id))
        .await?;

    let read_params = ReadThreadParams {
        thread_id,
        include_archived: false,
        include_history: true,
    };
    let from_writer = writer.read_thread(read_params.clone()).await?;
    let from_reader = reader.read_thread(read_params).await?;
    assert_eq!(
        serde_json::to_value(&from_reader)?,
        serde_json::to_value(&from_writer)?
    );
    assert_eq!(from_reader.thread_id, thread_id);
    assert_eq!(from_reader.rollout_path, None);
    assert_eq!(from_reader.model_provider, "postgres-test-provider");
    assert_eq!(from_reader.cwd, std::path::PathBuf::new());
    let history = from_reader.history.ok_or("history must be loaded")?;
    assert_eq!(history.thread_id, thread_id);
    assert_eq!(history.items.len(), 1);
    let session_meta = serde_json::to_value(&history.items[0])?;
    assert_eq!(session_meta["type"], "session_meta");
    assert_eq!(session_meta["payload"]["id"], thread_id.to_string());
    assert_eq!(session_meta["payload"]["session_id"], thread_id.to_string());
    assert_eq!(
        session_meta["payload"]["model_provider"],
        "postgres-test-provider"
    );

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

fn create_thread_params(thread_id: ThreadId) -> CreateThreadParams {
    CreateThreadParams {
        session_id: thread_id.into(),
        thread_id,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "postgres-contract".to_string(),
        base_instructions: BaseInstructions::default(),
        dynamic_tools: Vec::new(),
        selected_capability_roots: Vec::new(),
        multi_agent_version: None,
        history_mode: ThreadHistoryMode::Legacy,
        subagent_history_start_ordinal: None,
        initial_window_id: "postgres-contract-window".to_string(),
        metadata: ThreadPersistenceMetadata {
            cwd: None,
            model_provider: "postgres-test-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

fn append_items() -> Vec<RolloutItem> {
    vec![
        RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            client_id: Some("client-append-contract".to_string()),
            message: "full fidelity user message".to_string(),
            ..Default::default()
        })),
        RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput {
            id: Some(ResponseItemId::from_server("response-output-id".into())),
            call_id: "function-call-id".to_string(),
            output: FunctionCallOutputPayload::from_text("structured function output".to_string()),
            internal_chat_message_metadata_passthrough: None,
        }),
        RolloutItem::Compacted(CompactedItem {
            message: "compacted history marker".to_string(),
            replacement_history: None,
            window_number: Some(2),
            first_window_id: Some("window-first".to_string()),
            previous_window_id: Some("window-previous".to_string()),
            window_id: Some("window-current".to_string()),
        }),
    ]
}

struct PostgresThreadStoreFixture {
    config: PostgresNamespaceConfig,
    database_url: String,
    schema: String,
}

impl PostgresThreadStoreFixture {
    fn new(group: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let database_url = std::env::var(TEST_DATABASE_URL_ENV)?;
        let process_id = std::process::id();
        let sequence = NEXT_SCHEMA_ID.fetch_add(1, Ordering::Relaxed);
        let schema = format!("codex_thread_{group}_{process_id}_{sequence}");
        let config = PostgresNamespaceConfig::new(
            TEST_DATABASE_URL_ENV.to_string(),
            schema.clone(),
            PostgresPoolConfig::default(),
        )?;
        Ok(Self {
            config,
            database_url,
            schema,
        })
    }

    async fn migrate(&self) -> Result<(), Box<dyn std::error::Error>> {
        codex_state::manage_postgres_namespace(
            self.config.clone(),
            PostgresNamespaceAction::Migrate,
        )
        .await?;
        Ok(())
    }

    async fn cleanup(&self) -> Result<(), Box<dyn std::error::Error>> {
        let pool = sqlx::PgPool::connect(&self.database_url).await?;
        let schema = &self.schema;
        sqlx::query(AssertSqlSafe(format!("DROP SCHEMA \"{schema}\" CASCADE")))
            .execute(&pool)
            .await?;
        pool.close().await;
        Ok(())
    }
}
