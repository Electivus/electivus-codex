use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
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
use tempfile::TempDir;

use crate::AppendBatchId;
use crate::AppendThreadItemsBatch;
use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::CreateThreadParams;
use crate::DeleteThreadParams;
use crate::LoadThreadHistoryParams;
use crate::LocalThreadStore;
use crate::LocalThreadStoreConfig;
use crate::PostgresThreadStore;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::StoredThread;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;
use crate::ThreadStoreError;

const TEST_DATABASE_URL_ENV: &str = "CODEX_TEST_POSTGRES_URL";
static NEXT_SCHEMA_ID: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
async fn local_lifecycle_contract_matches_public_thread_store_semantics()
-> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let config = LocalThreadStoreConfig {
        codex_home: home.path().to_path_buf(),
        sqlite_home: home.path().to_path_buf(),
        default_model_provider_id: "lifecycle-contract-provider".to_string(),
    };
    let runtime = codex_state::StateRuntime::init(
        home.path().to_path_buf(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await?;
    let store = LocalThreadStore::new(config, Some(runtime));
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f101")?;

    assert_lifecycle_contract(&store, thread_id, home.path()).await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_lifecycle_contract_matches_public_thread_store_semantics()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresLifecycleFixture::new("shared")?;
    fixture.migrate().await?;
    let pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let store = PostgresThreadStore::new(&pool);
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f102")?;

    assert_lifecycle_contract(&store, thread_id, Path::new("/postgres-contract")).await?;

    pool.close().await;
    fixture.cleanup().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_lifecycle_contract_archive_serializes_with_local_appends()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresLifecycleFixture::new("archive_append_race")?;
    fixture.migrate().await?;
    let pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let store = PostgresThreadStore::new(&pool);

    for sequence in 0..64 {
        let thread_id = ThreadId::from_string(
            &uuid::Uuid::from_u128(0x0198c4cf85877d328d1c2c14d331f200 + sequence).to_string(),
        )?;
        store
            .create_thread(create_thread_params(
                thread_id,
                Path::new("/postgres-contract"),
            ))
            .await?;
        let (append_result, archive_result) = tokio::join!(
            store.append_items(AppendThreadItemsParams {
                thread_id,
                items: vec![user_message("append racing with archive")],
            }),
            store.archive_thread(ArchiveThreadParams { thread_id }),
        );
        archive_result?;
        assert!(
            append_result.is_ok()
                || matches!(append_result, Err(ThreadStoreError::Conflict { .. }))
                || matches!(append_result, Err(ThreadStoreError::ThreadNotFound { .. }))
        );
        let late_persist = store
            .persist_thread(thread_id)
            .await
            .expect_err("archive must remove the local writer");
        assert!(matches!(
            late_persist,
            ThreadStoreError::ThreadNotFound { .. }
        ));
    }

    pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_lifecycle_contract_fences_writers_and_delete_cascades_state()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresLifecycleFixture::new("fence_delete")?;
    fixture.migrate().await?;
    let writer_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let manager_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let writer = PostgresThreadStore::new(&writer_pool);
    let manager = PostgresThreadStore::new(&manager_pool);
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f103")?;
    let batch_id = AppendBatchId::new();
    writer
        .create_thread(create_thread_params(
            thread_id,
            Path::new("/postgres-contract"),
        ))
        .await?;
    writer
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            batch_id,
            vec![user_message("history deleted with original generation")],
        ))
        .await?;
    let before_archive = read_thread(&manager, thread_id, /*include_archived*/ false).await?;

    manager
        .archive_thread(ArchiveThreadParams { thread_id })
        .await?;
    let archived = read_thread(&manager, thread_id, /*include_archived*/ true).await?;
    assert_eq!(archived.updated_at, before_archive.updated_at);
    assert_eq!(archived.recency_at, before_archive.recency_at);
    let stale_append = writer
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![user_message("stale append after archive")],
        })
        .await
        .expect_err("archive must fence the active writer");
    assert!(matches!(stale_append, ThreadStoreError::Conflict { .. }));
    let unarchived = manager
        .unarchive_thread(ArchiveThreadParams { thread_id })
        .await?;
    assert!(unarchived.updated_at >= archived.updated_at);
    assert_eq!(unarchived.recency_at, archived.recency_at);
    writer
        .resume_thread(resume_thread_params(
            thread_id,
            Path::new("/postgres-contract"),
        ))
        .await?;

    manager
        .delete_thread(DeleteThreadParams { thread_id })
        .await?;
    let stale_append = writer
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![user_message("stale append after delete")],
        })
        .await
        .expect_err("delete must fence the active writer");
    assert!(matches!(
        stale_append,
        ThreadStoreError::ThreadNotFound { .. }
    ));

    writer
        .create_thread(create_thread_params(
            thread_id,
            Path::new("/postgres-contract"),
        ))
        .await?;
    let replacement = user_message("replacement generation history");
    writer
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            batch_id,
            vec![replacement.clone()],
        ))
        .await?;
    let history = manager
        .load_history(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    assert_eq!(history.items.len(), 2);
    assert_eq!(
        serde_json::to_value(history.items.last())?,
        serde_json::to_value(replacement)?
    );
    let replacement_thread = read_thread(&manager, thread_id, /*include_archived*/ false).await?;
    assert_eq!(replacement_thread.preview, "replacement generation history");

    writer_pool.close().await;
    manager_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_lifecycle_contract_concurrent_transitions_have_one_winner()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresLifecycleFixture::new("concurrent")?;
    fixture.migrate().await?;
    let first_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let second_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let first = PostgresThreadStore::new(&first_pool);
    let second = PostgresThreadStore::new(&second_pool);
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f104")?;
    create_inactive_thread(&first, thread_id, Path::new("/postgres-contract")).await?;

    let (first_archive, second_archive) = tokio::join!(
        first.archive_thread(ArchiveThreadParams { thread_id }),
        second.archive_thread(ArchiveThreadParams { thread_id }),
    );
    assert_eq!(
        usize::from(first_archive.is_ok()) + usize::from(second_archive.is_ok()),
        1
    );
    assert!(
        [first_archive.as_ref().err(), second_archive.as_ref().err()]
            .into_iter()
            .flatten()
            .all(|error| matches!(error, ThreadStoreError::InvalidRequest { .. }))
    );

    let (first_unarchive, second_unarchive) = tokio::join!(
        first.unarchive_thread(ArchiveThreadParams { thread_id }),
        second.unarchive_thread(ArchiveThreadParams { thread_id }),
    );
    assert_eq!(
        usize::from(first_unarchive.is_ok()) + usize::from(second_unarchive.is_ok()),
        1
    );
    assert!(
        [
            first_unarchive.as_ref().err(),
            second_unarchive.as_ref().err()
        ]
        .into_iter()
        .flatten()
        .all(|error| matches!(error, ThreadStoreError::InvalidRequest { .. }))
    );

    let (first_delete, second_delete) = tokio::join!(
        first.delete_thread(DeleteThreadParams { thread_id }),
        second.delete_thread(DeleteThreadParams { thread_id }),
    );
    assert_eq!(
        usize::from(first_delete.is_ok()) + usize::from(second_delete.is_ok()),
        1
    );
    assert!(
        [first_delete.as_ref().err(), second_delete.as_ref().err()]
            .into_iter()
            .flatten()
            .all(|error| matches!(error, ThreadStoreError::ThreadNotFound { .. }))
    );

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

async fn assert_lifecycle_contract(
    store: &dyn ThreadStore,
    thread_id: ThreadId,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let before_archive = create_inactive_thread(store, thread_id, cwd).await?;

    store
        .archive_thread(ArchiveThreadParams { thread_id })
        .await?;

    let active_only_error = store
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived: false,
            include_history: false,
        })
        .await
        .expect_err("active-only read must reject an archived thread");
    assert!(matches!(
        active_only_error,
        ThreadStoreError::InvalidRequest { .. }
    ));
    let archived = read_thread(store, thread_id, /*include_archived*/ true).await?;
    assert!(archived.archived_at.is_some());
    assert_eq!(archived.updated_at, before_archive.updated_at);
    assert!(archived.recency_at >= before_archive.recency_at);
    let duplicate_archive = store
        .archive_thread(ArchiveThreadParams { thread_id })
        .await
        .expect_err("an archived thread cannot be archived again");
    assert!(matches!(
        duplicate_archive,
        ThreadStoreError::InvalidRequest { .. }
    ));
    let archived_resume_error = store
        .resume_thread(resume_thread_params(thread_id, cwd))
        .await
        .expect_err("active-only resume must reject an archived thread");
    assert!(matches!(
        archived_resume_error,
        ThreadStoreError::InvalidRequest { .. }
    ));
    let mut archived_resume = resume_thread_params(thread_id, cwd);
    archived_resume.include_archived = true;
    store.resume_thread(archived_resume).await?;
    store.shutdown_thread(thread_id).await?;

    let unarchived = store
        .unarchive_thread(ArchiveThreadParams { thread_id })
        .await?;

    assert_eq!(unarchived.thread_id, thread_id);
    assert_eq!(unarchived.archived_at, None);
    assert!(unarchived.updated_at >= archived.updated_at);
    let duplicate_unarchive = store
        .unarchive_thread(ArchiveThreadParams { thread_id })
        .await
        .expect_err("an active thread cannot be unarchived");
    assert!(matches!(
        duplicate_unarchive,
        ThreadStoreError::InvalidRequest { .. }
    ));

    store
        .delete_thread(DeleteThreadParams { thread_id })
        .await?;
    let duplicate_delete = store
        .delete_thread(DeleteThreadParams { thread_id })
        .await
        .expect_err("a deleted thread cannot be deleted again");
    assert!(matches!(
        duplicate_delete,
        ThreadStoreError::ThreadNotFound { .. }
    ));
    Ok(())
}

async fn create_inactive_thread(
    store: &dyn ThreadStore,
    thread_id: ThreadId,
    cwd: &Path,
) -> Result<StoredThread, Box<dyn std::error::Error>> {
    store
        .create_thread(create_thread_params(thread_id, cwd))
        .await?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![user_message("lifecycle contract history")],
        })
        .await?;
    store.shutdown_thread(thread_id).await?;
    read_thread(store, thread_id, /*include_archived*/ false).await
}

async fn read_thread(
    store: &dyn ThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
) -> Result<StoredThread, Box<dyn std::error::Error>> {
    Ok(store
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived,
            include_history: false,
        })
        .await?)
}

fn create_thread_params(thread_id: ThreadId, cwd: &Path) -> CreateThreadParams {
    CreateThreadParams {
        session_id: thread_id.into(),
        thread_id,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "lifecycle-contract".to_string(),
        base_instructions: BaseInstructions::default(),
        dynamic_tools: Vec::new(),
        selected_capability_roots: Vec::new(),
        multi_agent_version: None,
        history_mode: ThreadHistoryMode::Legacy,
        subagent_history_start_ordinal: None,
        initial_window_id: "lifecycle-contract-window".to_string(),
        metadata: ThreadPersistenceMetadata {
            cwd: Some(cwd.to_path_buf()),
            model_provider: "lifecycle-contract-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

fn resume_thread_params(thread_id: ThreadId, cwd: &Path) -> ResumeThreadParams {
    ResumeThreadParams {
        thread_id,
        rollout_path: None,
        history: None,
        include_archived: false,
        metadata: ThreadPersistenceMetadata {
            cwd: Some(cwd.to_path_buf()),
            model_provider: "lifecycle-contract-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

fn user_message(message: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        message: message.to_string(),
        ..Default::default()
    }))
}

struct PostgresLifecycleFixture {
    config: PostgresNamespaceConfig,
    database_url: String,
    schema: String,
}

impl PostgresLifecycleFixture {
    fn new(group: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let database_url = std::env::var(TEST_DATABASE_URL_ENV)?;
        let sequence = NEXT_SCHEMA_ID.fetch_add(1, Ordering::Relaxed);
        let schema = format!("codex_lifecycle_{group}_{}_{sequence}", std::process::id());
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
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA \"{schema}\" CASCADE"
        )))
        .execute(&pool)
        .await?;
        pool.close().await;
        Ok(())
    }
}
