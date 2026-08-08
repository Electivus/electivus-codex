#![allow(warnings, clippy::all)]

use super::*;
use crate::list::parse_cursor;
use chrono::DateTime;
use chrono::NaiveDateTime;
use chrono::Timelike;
use chrono::Utc;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::UserMessageEvent;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

#[test]
fn cursor_to_anchor_normalizes_timestamp_format() {
    let ts_str = "2026-01-27T12-34-56";
    let cursor = parse_cursor(ts_str).expect("cursor should parse");
    let anchor = cursor_to_anchor(Some(&cursor)).expect("anchor should parse");

    let naive =
        NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H-%M-%S").expect("ts should parse");
    let expected_ts = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
        .with_nanosecond(0)
        .expect("nanosecond");

    assert_eq!(anchor.ts, expected_ts);
    assert_eq!(anchor.id, None);
}

#[test]
fn cursor_to_anchor_preserves_recency_tie_breaker() {
    let id = ThreadId::from_string("00000000-0000-0000-0000-000000000123")
        .expect("thread id should parse");
    let token = format!("2026-01-27T12:34:56Z|{id}");
    let cursor = parse_cursor(&token).expect("cursor should parse");
    let anchor = cursor_to_anchor(Some(&cursor)).expect("anchor should parse");

    assert_eq!(anchor.id, Some(id));
    assert_eq!(
        serde_json::to_string(&cursor).expect("cursor should serialize"),
        format!("\"{token}\"")
    );
}

/// A runtime for another SQLite home must not be queried or clean up rows when
/// a caller supplies a mismatched configuration.
#[tokio::test]
async fn list_threads_db_rejects_mismatched_sqlite_config_without_cleanup() -> anyhow::Result<()> {
    let root = TempDir::new().expect("temp dir");
    let runtime_sqlite = codex_state::SqliteConfig::new_for_testing(
        root.path().join("runtime-sqlite").as_path().abs(),
    );
    let requested_sqlite = codex_state::SqliteConfig::new_for_testing(
        root.path().join("requested-sqlite").as_path().abs(),
    );
    let runtime =
        codex_state::StateRuntime::init(runtime_sqlite, "test-provider".to_string()).await?;
    let thread_id = ThreadId::new();
    let metadata = ThreadMetadataBuilder::new(
        thread_id,
        root.path().join("missing-rollout.jsonl"),
        Utc::now(),
        SessionSource::Cli,
    )
    .build("test-provider");
    runtime.upsert_thread(&metadata).await?;

    let page = list_threads_db(
        Some(runtime.as_ref()),
        &requested_sqlite,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        /*repository_identity*/ None,
        /*relation_filter*/ None,
        /*archived*/ false,
        /*section*/ None,
        /*search_term*/ None,
    )
    .await;

    assert!(page.is_none());
    assert_eq!(runtime.get_thread(thread_id).await?, Some(metadata));
    Ok(())
}

#[tokio::test]
async fn try_init_waits_for_concurrent_startup_backfill() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
    )
    .await?;
    let coordinator = runtime.backfill_coordinator();
    let lease = match coordinator
        .try_claim("concurrent-worker", Duration::from_secs(60))
        .await?
    {
        codex_state::BackfillClaimOutcome::Claimed { lease, .. } => lease,
        outcome => anyhow::bail!("expected claimed lease, got {outcome:?}"),
    };
    let coordinator_for_completion = coordinator.clone();
    let complete_backfill = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        coordinator_for_completion
            .complete(&lease, /*last_watermark*/ None)
            .await
    });

    let initialized = try_init_with_roots_and_backfill_lease(
        home.path().to_path_buf(),
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
        /*backfill_lease_duration*/ Duration::from_secs(60),
    )
    .await?;
    complete_backfill.await??;
    assert_eq!(
        initialized.backfill_coordinator().state().await?.status,
        codex_state::BackfillStatus::Complete
    );

    Ok(())
}

#[tokio::test]
async fn try_init_times_out_waiting_for_stuck_startup_backfill() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
    )
    .await?;
    let _lease = match runtime
        .backfill_coordinator()
        .try_claim("stuck-worker", Duration::from_secs(60))
        .await?
    {
        codex_state::BackfillClaimOutcome::Claimed { lease, .. } => lease,
        outcome => anyhow::bail!("expected claimed lease, got {outcome:?}"),
    };

    let result = try_init_with_roots_and_backfill_lease(
        home.path().to_path_buf(),
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
        /*backfill_lease_duration*/ Duration::from_secs(60),
    )
    .await;
    let err = match result {
        Ok(_) => panic!("state db init should not wait forever for incomplete backfill"),
        Err(err) => err,
    };
    assert!(
        err.to_string()
            .contains("timed out waiting for state db backfill"),
        "unexpected error: {err}"
    );

    Ok(())
}

#[tokio::test]
async fn reconcile_rollout_preserves_existing_explicit_title() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::new();
    let rollout_path =
        write_rollout_with_user_message(home.path(), thread_id, "Hey", ThreadHistoryMode::Legacy)?;
    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
    )
    .await?;

    let mut metadata =
        metadata::extract_metadata_from_rollout(rollout_path.as_path(), "test-provider")
            .await?
            .metadata;
    assert_eq!(metadata.title, "Hey");
    assert_eq!(metadata.first_user_message.as_deref(), Some("Hey"));
    metadata.title = "math".to_string();
    runtime.upsert_thread(&metadata).await?;

    reconcile_rollout(
        Some(runtime.as_ref()),
        rollout_path.as_path(),
        "test-provider",
        /*builder*/ None,
        &[],
        /*archived_only*/ Some(false),
        /*new_thread_memory_mode*/ None,
    )
    .await;

    let persisted = runtime
        .get_thread(thread_id)
        .await?
        .expect("thread should exist");
    assert_eq!(persisted.title, "math");
    assert_eq!(persisted.first_user_message.as_deref(), Some("Hey"));
    Ok(())
}

#[tokio::test]
async fn checked_reconcile_reports_decoded_source_bytes_after_persisting() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::new();
    let rollout_path =
        write_rollout_with_user_message(home.path(), thread_id, "Hey", ThreadHistoryMode::Legacy)?;
    let source_bytes = std::fs::metadata(&rollout_path)?.len();
    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
    )
    .await?;

    let consumed_source_bytes = reconcile_rollout_checked(
        runtime.as_ref(),
        rollout_path.as_path(),
        "test-provider",
        /*archived_only*/ Some(false),
        /*maximum_source_bytes*/ source_bytes,
    )
    .await?;

    assert_eq!(consumed_source_bytes, source_bytes);
    assert!(runtime.get_thread(thread_id).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn checked_reconcile_propagates_source_budget_and_parse_failures() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
    )
    .await?;

    let oversized_id = ThreadId::new();
    let oversized_path = write_rollout_with_user_message(
        home.path(),
        oversized_id,
        "oversized",
        ThreadHistoryMode::Legacy,
    )?;
    let source_bytes = std::fs::metadata(&oversized_path)?.len();
    let oversized = reconcile_rollout_checked(
        runtime.as_ref(),
        &oversized_path,
        "test-provider",
        Some(false),
        source_bytes - 1,
    )
    .await
    .expect_err("source budget should be enforced");
    assert!(oversized.to_string().contains("decoded-byte budget"));
    assert!(runtime.get_thread(oversized_id).await?.is_none());

    let invalid_id = ThreadId::new();
    let invalid_path = write_rollout_with_user_message(
        home.path(),
        invalid_id,
        "valid",
        ThreadHistoryMode::Legacy,
    )?;
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&invalid_path)?,
        "not-json"
    )?;
    let invalid = reconcile_rollout_checked(
        runtime.as_ref(),
        &invalid_path,
        "test-provider",
        Some(false),
        u64::MAX,
    )
    .await
    .expect_err("parse failure should be propagated");
    assert!(invalid.to_string().contains("invalid record"));
    assert!(runtime.get_thread(invalid_id).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn reconcile_rollout_preserves_existing_paginated_memory_mode() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::new();
    let rollout_path = write_rollout_with_user_message(
        home.path(),
        thread_id,
        "Hey",
        ThreadHistoryMode::Paginated,
    )?;
    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
    )
    .await?;

    reconcile_rollout(
        Some(runtime.as_ref()),
        rollout_path.as_path(),
        "test-provider",
        /*builder*/ None,
        &[],
        /*archived_only*/ None,
        /*new_thread_memory_mode*/ None,
    )
    .await;
    assert!(
        runtime
            .set_thread_memory_mode(thread_id, "disabled")
            .await?
    );

    reconcile_rollout(
        Some(runtime.as_ref()),
        rollout_path.as_path(),
        "test-provider",
        /*builder*/ None,
        &[],
        /*archived_only*/ None,
        /*new_thread_memory_mode*/ None,
    )
    .await;

    assert_eq!(
        runtime.get_thread_memory_mode(thread_id).await?.as_deref(),
        Some("disabled")
    );
    Ok(())
}

fn write_rollout_with_user_message(
    home: &Path,
    thread_id: ThreadId,
    message: &str,
    history_mode: ThreadHistoryMode,
) -> anyhow::Result<std::path::PathBuf> {
    let dir = home.join("sessions/2026/06/01");
    std::fs::create_dir_all(dir.as_path())?;
    let path = dir.join(format!("rollout-2026-06-01T14-26-25-{thread_id}.jsonl"));
    let lines = [
        RolloutLine {
            timestamp: "2026-06-01T14:26:25Z".to_string(),
            ordinal: None,
            item: RolloutItem::SessionMeta(SessionMetaLine {
                meta: SessionMeta {
                    session_id: thread_id.into(),
                    id: thread_id,
                    forked_from_id: None,
                    parent_thread_id: None,
                    timestamp: "2026-06-01T14:26:25Z".to_string(),
                    cwd: home.to_path_buf(),
                    originator: "test".to_string(),
                    cli_version: "test".to_string(),
                    source: SessionSource::Cli,
                    thread_source: None,
                    agent_nickname: None,
                    agent_role: None,
                    agent_path: None,
                    model_provider: Some("test-provider".to_string()),
                    base_instructions: None,
                    dynamic_tools: None,
                    selected_capability_roots: Vec::new(),
                    memory_mode: None,
                    history_mode,
                    history_base: None,
                    subagent_history_start_ordinal: None,
                    multi_agent_version: None,
                    context_window: None,
                },
                git: None,
            }),
        },
        RolloutLine {
            timestamp: "2026-06-01T14:26:26Z".to_string(),
            ordinal: None,
            item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: message.to_string(),
                ..Default::default()
            })),
        },
    ];
    let jsonl = lines
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    std::fs::write(path.as_path(), format!("{jsonl}\n"))?;
    Ok(path)
}
