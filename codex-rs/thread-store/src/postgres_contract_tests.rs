#![allow(
    clippy::disallowed_methods,
    reason = "PostgreSQL tests connect only to PostgreSQL pools"
)]

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::UserMessageEvent;
use codex_rollout::CompactedItem;
use codex_rollout::RolloutItem;
use codex_state::PostgresNamespaceAction;
use codex_state::PostgresNamespaceConfig;
use codex_state::PostgresPoolConfig;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;

use crate::AppendBatchId;
use crate::AppendThreadItemsBatch;
use crate::CreateThreadParams;
use crate::GitInfoPatch;
use crate::PersistContext;
use crate::PostgresThreadStore;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::ThreadMetadataPatch;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;
use crate::UpdateThreadMetadataParams;

const TEST_DATABASE_URL_ENV: &str = "CODEX_TEST_POSTGRES_URL";
static NEXT_SCHEMA_ID: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_metadata_updates_and_repairs_from_canonical_history()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_metadata")?;
    fixture.migrate().await?;
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let replica = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f010")?;
    writer
        .create_thread(create_thread_params(thread_id))
        .await?;
    writer
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            append_items(),
        ))
        .await?;
    let before_update = replica
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived: false,
            include_history: false,
        })
        .await?;
    let updated_at = before_update.updated_at + chrono::Duration::seconds(10);
    let recency_at = before_update.recency_at + chrono::Duration::seconds(20);

    let updated = writer
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch: ThreadMetadataPatch {
                name: Some(Some("Shared PostgreSQL thread".to_string())),
                updated_at: Some(updated_at),
                advance_recency_at: Some(recency_at),
                git_info: Some(GitInfoPatch {
                    sha: Some(Some("abc123".to_string())),
                    branch: Some(Some("main".to_string())),
                    origin_url: Some(Some("ssh://git@example.com/acme/codex.git".to_string())),
                }),
                memory_mode: Some(ThreadMemoryMode::Disabled),
                ..Default::default()
            },
            include_archived: false,
        })
        .await?
        .expect("existing PostgreSQL thread should be updated");
    let from_replica = replica
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived: false,
            include_history: false,
        })
        .await?;
    assert_eq!(
        serde_json::to_value(&from_replica)?,
        serde_json::to_value(&updated)?
    );
    assert_eq!(
        from_replica.repository_identity.as_deref(),
        Some("example.com/acme/codex")
    );
    assert_eq!(
        from_replica.name.as_deref(),
        Some("Shared PostgreSQL thread")
    );
    assert_eq!(from_replica.updated_at, updated_at);
    assert_eq!(from_replica.recency_at, recency_at);
    let updated_history = replica
        .load_history(crate::LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    let latest_meta = updated_history
        .items
        .iter()
        .rev()
        .find_map(|item| match item {
            RolloutItem::SessionMeta(meta) => Some(meta),
            _ => None,
        })
        .ok_or("updated canonical SessionMeta must exist")?;
    assert_eq!(latest_meta.meta.memory_mode.as_deref(), Some("disabled"));
    assert_eq!(
        serde_json::to_value(&latest_meta.git)?,
        serde_json::json!({
            "commit_hash": "abc123",
            "branch": "main",
            "repository_url": "ssh://git@example.com/acme/codex.git",
        })
    );

    let partially_updated = writer
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch: ThreadMetadataPatch {
                git_info: Some(GitInfoPatch {
                    branch: Some(Some("feature/postgres".to_string())),
                    ..Default::default()
                }),
                ..Default::default()
            },
            include_archived: false,
        })
        .await?
        .expect("existing PostgreSQL thread should be partially updated");
    assert_eq!(
        serde_json::to_value(&partially_updated.git_info)?,
        serde_json::json!({
            "commit_hash": "abc123",
            "branch": "feature/postgres",
            "repository_url": "ssh://git@example.com/acme/codex.git",
        })
    );
    let cleared = writer
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch: ThreadMetadataPatch {
                git_info: Some(GitInfoPatch {
                    sha: Some(None),
                    branch: Some(None),
                    origin_url: Some(None),
                }),
                ..Default::default()
            },
            include_archived: false,
        })
        .await?
        .expect("existing PostgreSQL thread Git metadata should be cleared");
    assert!(cleared.git_info.is_none());
    let memory_only = writer
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch: ThreadMetadataPatch {
                memory_mode: Some(ThreadMemoryMode::Enabled),
                ..Default::default()
            },
            include_archived: false,
        })
        .await?
        .expect("existing PostgreSQL thread memory mode should be updated");
    assert!(memory_only.git_info.is_none());
    let metadata_markers = replica
        .load_history(crate::LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?
        .items
        .into_iter()
        .rev()
        .filter_map(|item| match item {
            RolloutItem::SessionMeta(meta) => Some(meta),
            _ => None,
        })
        .take(2)
        .collect::<Vec<_>>();
    assert_eq!(
        metadata_markers[0].meta.memory_mode.as_deref(),
        Some("enabled")
    );
    assert!(metadata_markers[0].git.is_none());
    assert!(metadata_markers[1].git.is_some());
    assert_eq!(
        serde_json::to_value(&metadata_markers[1].git)?,
        serde_json::json!({})
    );

    let mut expected_repaired = memory_only;
    expected_repaired.name = Some("Repaired from history".to_string());
    fixture.damage_thread_projection(thread_id).await?;
    let repaired = replica
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch: ThreadMetadataPatch {
                name: Some(Some("Repaired from history".to_string())),
                ..Default::default()
            },
            include_archived: false,
        })
        .await?
        .expect("existing PostgreSQL thread projection should be repaired");
    assert_eq!(
        serde_json::to_value(repaired)?,
        serde_json::to_value(expected_repaired)?
    );

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_concurrent_metadata_updates_do_not_regress_timestamps()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_metadata_concurrent")?;
    fixture.migrate().await?;
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let first = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let second = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f013")?;
    first.create_thread(create_thread_params(thread_id)).await?;
    first.shutdown_thread(thread_id).await?;
    let initial = first
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived: false,
            include_history: false,
        })
        .await?;
    let updated_at = initial.updated_at + chrono::Duration::days(1);
    let earlier_recency_at = initial.recency_at + chrono::Duration::days(1);
    let recency_at = initial.recency_at + chrono::Duration::days(2);
    first
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch: ThreadMetadataPatch {
                updated_at: Some(updated_at),
                ..Default::default()
            },
            include_archived: false,
        })
        .await?;

    let (sha_result, branch_result) = tokio::join!(
        first.update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch: ThreadMetadataPatch {
                git_info: Some(GitInfoPatch {
                    sha: Some(Some("concurrent-sha".to_string())),
                    ..Default::default()
                }),
                advance_recency_at: Some(earlier_recency_at),
                ..Default::default()
            },
            include_archived: false,
        }),
        second.update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch: ThreadMetadataPatch {
                git_info: Some(GitInfoPatch {
                    branch: Some(Some("concurrent-branch".to_string())),
                    ..Default::default()
                }),
                advance_recency_at: Some(recency_at),
                ..Default::default()
            },
            include_archived: false,
        }),
    );
    sha_result?;
    branch_result?;

    let final_thread = first
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived: false,
            include_history: false,
        })
        .await?;
    assert_eq!(final_thread.updated_at, updated_at);
    assert_eq!(final_thread.recency_at, recency_at);
    assert_eq!(
        serde_json::to_value(final_thread.git_info)?,
        serde_json::json!({
            "commit_hash": "concurrent-sha",
            "branch": "concurrent-branch",
        })
    );

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_concurrent_resume_acquires_exactly_one_writer()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_resume_concurrent")?;
    fixture.migrate().await?;
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let first = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let second = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f009")?;
    first.create_thread(create_thread_params(thread_id)).await?;
    first.shutdown_thread(thread_id).await?;

    let (first_result, second_result) = tokio::join!(
        first.resume_thread(resume_thread_params(thread_id)),
        second.resume_thread(resume_thread_params(thread_id)),
    );
    let acquired = match (first_result, second_result) {
        (Ok(()), Err(crate::ThreadStoreError::Conflict { .. })) => &first,
        (Err(crate::ThreadStoreError::Conflict { .. }), Ok(())) => &second,
        results => panic!("exactly one resume must acquire the writer, got {results:?}"),
    };
    acquired
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            append_items(),
        ))
        .await?;

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_ambiguous_append_retry_requires_current_writer_fence()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_resume_retry")?;
    fixture.migrate().await?;
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let superseded = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let current = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f008")?;
    superseded
        .create_thread(create_thread_params(thread_id))
        .await?;
    let batch_id = AppendBatchId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331ba04")?;
    let batch = AppendThreadItemsBatch::new(thread_id, batch_id, append_items());
    let original = superseded.append_batch(batch.clone()).await?;
    fixture.expire_writer_lease(thread_id).await?;
    current
        .resume_thread(resume_thread_params(thread_id))
        .await?;

    let stale_retry = superseded
        .append_batch(batch.clone())
        .await
        .expect_err("an old fence cannot replay even an identical append batch");
    assert!(matches!(
        stale_retry,
        crate::ThreadStoreError::Conflict { .. }
    ));
    assert_eq!(current.append_batch(batch.clone()).await?, original);
    let divergent_retry = current
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            batch_id,
            vec![RolloutItem::Compacted(CompactedItem {
                message: "different retry content".to_string(),
                replacement_history: None,
                mcp_resource_origins: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
            })],
        ))
        .await
        .expect_err("a current writer cannot reuse the key with divergent content");
    assert!(matches!(
        divergent_retry,
        crate::ThreadStoreError::Conflict { .. }
    ));
    let next = current
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            append_items(),
        ))
        .await?;
    assert_eq!(
        next,
        crate::AppendBatchCommit {
            first_ordinal: 4,
            persisted_item_count: 3,
            committed_stream_version: 7,
        }
    );

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_expired_writer_is_fenced_after_takeover()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_resume_expired")?;
    fixture.migrate().await?;
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let expired = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let takeover = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f007")?;
    expired
        .create_thread(create_thread_params(thread_id))
        .await?;
    fixture.expire_writer_lease(thread_id).await?;

    takeover
        .resume_thread(resume_thread_params(thread_id))
        .await?;
    let stale_append = expired
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            append_items(),
        ))
        .await
        .expect_err("a superseded writer must not append");
    let stale_renew = expired
        .flush_thread(thread_id)
        .await
        .expect_err("a superseded writer must not renew");
    let stale_release = expired
        .shutdown_thread(thread_id)
        .await
        .expect_err("a superseded writer must not release the new lease");
    assert!(matches!(
        (stale_append, stale_renew, stale_release),
        (
            crate::ThreadStoreError::Conflict { .. },
            crate::ThreadStoreError::Conflict { .. },
            crate::ThreadStoreError::Conflict { .. }
        )
    ));
    let committed = takeover
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            append_items(),
        ))
        .await?;
    assert_eq!(
        committed,
        crate::AppendBatchCommit {
            first_ordinal: 1,
            persisted_item_count: 3,
            committed_stream_version: 4,
        }
    );

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_resume_after_shutdown_uses_durable_stream_version()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_resume_shutdown")?;
    fixture.migrate().await?;
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let first = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let resumed = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f006")?;
    first.create_thread(create_thread_params(thread_id)).await?;
    first
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            append_items(),
        ))
        .await?;
    first.shutdown_thread(thread_id).await?;

    resumed
        .resume_thread(resume_thread_params(thread_id))
        .await?;
    let committed = resumed
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            append_items(),
        ))
        .await?;
    assert_eq!(
        committed,
        crate::AppendBatchCommit {
            first_ordinal: 4,
            persisted_item_count: 3,
            committed_stream_version: 7,
        }
    );
    assert_eq!(
        first
            .load_history(crate::LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await?
            .items
            .len(),
        7
    );

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_resume_conflicts_while_another_writer_lease_is_active()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_resume_conflict")?;
    fixture.migrate().await?;
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let contender = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f005")?;
    writer
        .create_thread(create_thread_params(thread_id))
        .await?;

    let error = contender
        .resume_thread(resume_thread_params(thread_id))
        .await
        .expect_err("a live writer lease must reject a second writer");
    assert!(matches!(error, crate::ThreadStoreError::Conflict { .. }));
    writer
        .append_batch(AppendThreadItemsBatch::new(
            thread_id,
            AppendBatchId::new(),
            append_items(),
        ))
        .await?;

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_flush_and_shutdown_preserve_durable_history()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("thread_lifecycle")?;
    fixture.migrate().await?;
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let reader = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
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

    writer
        .persist_thread(thread_id, PersistContext::Standard)
        .await?;
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
    let pool = fixture.connect_pool().await?;
    let store = PostgresThreadStore::new(pool.clone(), fixture.schema.clone());
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
                mcp_resource_origins: None,
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
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let reader = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
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
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(first_pool.clone(), fixture.schema.clone());
    let reader = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
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

pub(super) fn create_thread_params(thread_id: ThreadId) -> CreateThreadParams {
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
        history_base: None,
        subagent_history_start_ordinal: None,
        initial_window_id: "postgres-contract-window".to_string(),
        metadata: ThreadPersistenceMetadata {
            cwd: None,
            model_provider: "postgres-test-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

fn resume_thread_params(thread_id: ThreadId) -> ResumeThreadParams {
    ResumeThreadParams {
        thread_id,
        rollout_path: None,
        history: None,
        include_archived: false,
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
        RolloutItem::ResponseItem(
            ResponseItem::FunctionCallOutput {
                id: Some(ResponseItemId::from_server("response-output-id".into())),
                call_id: Some("function-call-id".to_string()),
                name: None,
                namespace: None,
                output: FunctionCallOutputPayload::from_text(
                    "structured function output".to_string(),
                ),
                internal_chat_message_metadata_passthrough: None,
            }
            .into(),
        ),
        RolloutItem::Compacted(CompactedItem {
            message: "compacted history marker".to_string(),
            replacement_history: None,
            mcp_resource_origins: None,
            window_number: Some(2),
            first_window_id: Some("window-first".to_string()),
            previous_window_id: Some("window-previous".to_string()),
            window_id: Some("window-current".to_string()),
        }),
    ]
}

pub(super) struct PostgresThreadStoreFixture {
    pub(super) config: PostgresNamespaceConfig,
    pub(super) database_url: String,
    pub(super) schema: String,
}

impl PostgresThreadStoreFixture {
    pub(super) fn new(group: &str) -> Result<Self, Box<dyn std::error::Error>> {
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

    pub(super) async fn migrate(&self) -> Result<(), Box<dyn std::error::Error>> {
        codex_state::manage_postgres_namespace(
            self.config.clone(),
            PostgresNamespaceAction::Migrate,
        )
        .await?;
        Ok(())
    }

    pub(super) async fn connect_pool(&self) -> Result<sqlx::PgPool, Box<dyn std::error::Error>> {
        Ok(sqlx::PgPool::connect(&self.database_url).await?)
    }

    async fn expire_writer_lease(
        &self,
        thread_id: ThreadId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let pool = sqlx::PgPool::connect(&self.database_url).await?;
        let schema = &self.schema;
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE \"{schema}\".threads \
             SET writer_lease_expires_at = CURRENT_TIMESTAMP - INTERVAL '1 second' \
             WHERE thread_id = $1"
        )))
        .bind(thread_id.to_string())
        .execute(&pool)
        .await?;
        pool.close().await;
        Ok(())
    }

    async fn damage_thread_projection(
        &self,
        thread_id: ThreadId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let pool = sqlx::PgPool::connect(&self.database_url).await?;
        let schema = &self.schema;
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE \"{schema}\".threads SET projection = '{{}}'::jsonb WHERE thread_id = $1"
        )))
        .bind(thread_id.to_string())
        .execute(&pool)
        .await?;
        pool.close().await;
        Ok(())
    }

    pub(super) async fn cleanup(&self) -> Result<(), Box<dyn std::error::Error>> {
        let pool = sqlx::PgPool::connect(&self.database_url).await?;
        let schema = &self.schema;
        sqlx::query(AssertSqlSafe(format!("DROP SCHEMA \"{schema}\" CASCADE")))
            .execute(&pool)
            .await?;
        pool.close().await;
        Ok(())
    }
}
