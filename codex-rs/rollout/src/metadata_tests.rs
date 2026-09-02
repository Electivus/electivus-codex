#![allow(warnings, clippy::all)]

use super::*;
use crate::CompactedItem;
use crate::RolloutItem;
use crate::RolloutLine;
use chrono::DateTime;
use chrono::NaiveDateTime;
use chrono::Timelike;
use chrono::Utc;
use codex_protocol::SanitizedGitUrl;
use codex_protocol::ThreadId;
use codex_protocol::protocol::GitInfo;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_state::BackfillClaimOutcome;
use codex_state::BackfillLeaseUpdate;
use codex_state::BackfillStatus;
use codex_state::ThreadMetadataBuilder;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::tempdir;
use uuid::Uuid;

#[test]
fn fork_cutoff_distinguishes_logical_parent_from_reverted_rollout() {
    let parent_id = ThreadId::new();
    let thread_id = ThreadId::new();
    let physical_id = ThreadId::new();
    let replacement_id = ThreadId::new();
    let original_path = PathBuf::from(format!("rollout-2026-01-27T12-34-56-{thread_id}.jsonl"));
    let reverted_path = PathBuf::from(format!(
        "rollout-2026-01-27T12-34-56-{thread_id}_{replacement_id}.jsonl"
    ));

    for (name, parent, cutoff, base_id, path, expected) in [
        (
            "persisted revert",
            Some(parent_id),
            Some(20),
            thread_id,
            Some(reverted_path.as_path()),
            Some(20),
        ),
        (
            "legacy direct fork",
            Some(parent_id),
            None,
            parent_id,
            None,
            Some(40),
        ),
        (
            "legacy fork of reverted parent",
            Some(parent_id),
            None,
            physical_id,
            Some(original_path.as_path()),
            Some(40),
        ),
        (
            "legacy child revert",
            Some(parent_id),
            None,
            thread_id,
            Some(reverted_path.as_path()),
            None,
        ),
        (
            "legacy repeated revert",
            Some(parent_id),
            None,
            physical_id,
            Some(reverted_path.as_path()),
            None,
        ),
        (
            "missing parent",
            None,
            Some(20),
            parent_id,
            Some(original_path.as_path()),
            None,
        ),
    ] {
        let meta = SessionMeta {
            id: thread_id,
            forked_from_id: parent,
            forked_from_ordinal_exclusive: cutoff,
            history_base: Some(HistoryPosition {
                thread_id: base_id,
                end_ordinal_exclusive: 40,
                end_byte_offset: 100,
            }),
            ..SessionMeta::default()
        };
        assert_eq!(
            forked_from_ordinal_exclusive(&meta, path),
            expected,
            "{name}"
        );
    }
}

#[tokio::test]
async fn extract_metadata_from_rollout_uses_session_meta() {
    let dir = tempdir().expect("tempdir");
    let uuid = Uuid::new_v4();
    let id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let path = dir
        .path()
        .join(format!("rollout-2026-01-27T12-34-56-{uuid}.jsonl"));

    let session_meta = SessionMeta {
        session_id: id.into(),
        id,
        forked_from_id: None,
        forked_from_ordinal_exclusive: None,
        parent_thread_id: None,
        timestamp: "2026-01-27T12:34:56Z".to_string(),
        cwd: dir.path().to_path_buf(),
        originator: "cli".to_string(),
        cli_version: "0.0.0".to_string(),
        source: SessionSource::default(),
        thread_source: None,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
        model_provider: Some("openai".to_string()),
        base_instructions: None,
        dynamic_tools: None,
        selected_capability_roots: Vec::new(),
        memory_mode: None,
        history_mode: ThreadHistoryMode::Paginated,
        history_base: None,
        subagent_history_start_ordinal: None,
        multi_agent_version: None,
        context_window: None,
    };
    let session_meta_line = SessionMetaLine {
        meta: session_meta,
        git: None,
    };
    let rollout_line = RolloutLine {
        timestamp: "2026-01-27T12:34:56Z".to_string(),
        ordinal: Some(0),
        item: RolloutItem::SessionMeta(session_meta_line.clone()),
    };
    let json = serde_json::to_string(&rollout_line).expect("rollout json");
    let mut file = File::create(&path).expect("create rollout");
    writeln!(file, "{json}").expect("write rollout");

    let outcome = extract_metadata_from_rollout(&path, "openai")
        .await
        .expect("extract");

    let builder = builder_from_session_meta(&session_meta_line, path.as_path()).expect("builder");
    let mut expected = builder.build("openai");
    apply_rollout_item(&mut expected, &rollout_line.item, "openai");
    expected.updated_at = file_modified_time_utc(&path).await.expect("mtime");
    expected.recency_at = expected.updated_at;

    assert_eq!(outcome.metadata, expected);
    assert_eq!(outcome.memory_mode, None);
    assert_eq!(outcome.parse_errors, 0);
}

#[tokio::test]
async fn extract_metadata_from_rollout_rejects_unknown_history_mode() {
    let dir = tempdir().expect("tempdir");
    let uuid = Uuid::new_v4();
    let id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let path = dir
        .path()
        .join(format!("rollout-2026-01-27T12-34-56-{uuid}.jsonl"));
    let mut rollout_line = serde_json::to_value(RolloutLine {
        timestamp: "2026-01-27T12:34:56Z".to_string(),
        ordinal: None,
        item: RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: id.into(),
                id,
                timestamp: "2026-01-27T12:34:56Z".to_string(),
                cwd: dir.path().to_path_buf(),
                originator: "cli".to_string(),
                cli_version: "0.0.0".to_string(),
                ..SessionMeta::default()
            },
            git: None,
        }),
    })
    .expect("serialize rollout line");
    rollout_line["payload"]["history_mode"] = serde_json::json!("future");
    let mut file = File::create(&path).expect("create rollout");
    writeln!(file, "{rollout_line}").expect("write rollout");

    assert!(
        extract_metadata_from_rollout(&path, "openai")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn extract_metadata_from_rollout_returns_latest_memory_mode() {
    let dir = tempdir().expect("tempdir");
    let uuid = Uuid::new_v4();
    let id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let path = dir
        .path()
        .join(format!("rollout-2026-01-27T12-34-56-{uuid}.jsonl"));

    let session_meta = SessionMeta {
        session_id: id.into(),
        id,
        forked_from_id: None,
        forked_from_ordinal_exclusive: None,
        parent_thread_id: None,
        timestamp: "2026-01-27T12:34:56Z".to_string(),
        cwd: dir.path().to_path_buf(),
        originator: "cli".to_string(),
        cli_version: "0.0.0".to_string(),
        source: SessionSource::default(),
        thread_source: None,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
        model_provider: Some("openai".to_string()),
        base_instructions: None,
        dynamic_tools: None,
        selected_capability_roots: Vec::new(),
        memory_mode: None,
        history_mode: Default::default(),
        history_base: None,
        subagent_history_start_ordinal: None,
        multi_agent_version: None,
        context_window: None,
    };
    let polluted_meta = SessionMeta {
        memory_mode: Some("polluted".to_string()),
        multi_agent_version: None,
        ..session_meta.clone()
    };
    let ancestor_meta = SessionMeta {
        id: ThreadId::from_string(&Uuid::new_v4().to_string()).expect("ancestor thread id"),
        memory_mode: Some("disabled".to_string()),
        ..session_meta.clone()
    };
    let lines = vec![
        RolloutLine {
            timestamp: "2026-01-27T12:34:56Z".to_string(),
            ordinal: None,
            item: RolloutItem::SessionMeta(SessionMetaLine {
                meta: session_meta,
                git: None,
            }),
        },
        RolloutLine {
            timestamp: "2026-01-27T12:35:00Z".to_string(),
            ordinal: None,
            item: RolloutItem::SessionMeta(SessionMetaLine {
                meta: polluted_meta,
                git: None,
            }),
        },
        RolloutLine {
            timestamp: "2026-01-27T12:36:00Z".to_string(),
            ordinal: None,
            item: RolloutItem::SessionMeta(SessionMetaLine {
                meta: ancestor_meta,
                git: None,
            }),
        },
    ];
    let mut file = File::create(&path).expect("create rollout");
    for line in lines {
        writeln!(
            file,
            "{}",
            serde_json::to_string(&line).expect("serialize rollout line")
        )
        .expect("write rollout line");
    }

    let outcome = extract_metadata_from_rollout(&path, "openai")
        .await
        .expect("extract");

    assert_eq!(outcome.memory_mode.as_deref(), Some("polluted"));
}

#[test]
fn builder_from_items_falls_back_to_filename() {
    let dir = tempdir().expect("tempdir");
    let uuid = Uuid::new_v4();
    let path = dir
        .path()
        .join(format!("rollout-2026-01-27T12-34-56-{uuid}.jsonl"));
    let items = vec![RolloutItem::Compacted(CompactedItem {
        message: "noop".to_string(),
        replacement_history: None,
        mcp_resource_origins: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    })];

    let builder = builder_from_items(items.as_slice(), path.as_path()).expect("builder");
    let naive = NaiveDateTime::parse_from_str("2026-01-27T12-34-56", "%Y-%m-%dT%H-%M-%S")
        .expect("timestamp");
    let created_at = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
        .with_nanosecond(0)
        .expect("nanosecond");
    let expected = ThreadMetadataBuilder::new(
        ThreadId::from_string(&uuid.to_string()).expect("thread id"),
        path,
        created_at,
        SessionSource::default(),
    );

    assert_eq!(builder, expected);
}

#[tokio::test]
async fn backfill_sessions_resumes_from_watermark_and_marks_complete() {
    let dir = tempdir().expect("tempdir");
    let codex_home = dir.path().to_path_buf();
    let first_uuid = Uuid::new_v4();
    let second_uuid = Uuid::new_v4();
    let first_path = write_rollout_in_sessions(
        codex_home.as_path(),
        "2026-01-27T12-34-56",
        "2026-01-27T12:34:56Z",
        first_uuid,
        /*git*/ None,
    );
    let second_path = write_rollout_in_sessions(
        codex_home.as_path(),
        "2026-01-27T12-35-56",
        "2026-01-27T12:35:56Z",
        second_uuid,
        /*git*/ None,
    );

    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("initialize runtime");
    let first_watermark = backfill_watermark_for_path(codex_home.as_path(), first_path.as_path());
    let coordinator = runtime.backfill_coordinator();
    let lease = match coordinator
        .try_claim("previous-worker", Duration::from_secs(60))
        .await
        .expect("claim previous worker")
    {
        BackfillClaimOutcome::Claimed { lease, .. } => lease,
        outcome => panic!("expected claimed lease, got {outcome:?}"),
    };
    assert_eq!(
        coordinator
            .checkpoint(&lease, first_watermark.as_str(), Duration::ZERO)
            .await
            .expect("checkpoint first watermark"),
        BackfillLeaseUpdate::Applied
    );

    let outcome = backfill_sessions_with_lease(
        runtime.as_ref(),
        codex_home.as_path(),
        "test-provider",
        Duration::from_secs(60),
        "resume-worker",
    )
    .await;
    assert_eq!(outcome, BackfillRunOutcome::Completed);

    let first_id = ThreadId::from_string(&first_uuid.to_string()).expect("first thread id");
    let second_id = ThreadId::from_string(&second_uuid.to_string()).expect("second thread id");
    assert_eq!(
        runtime
            .get_thread(first_id)
            .await
            .expect("get first thread"),
        None
    );
    assert!(
        runtime
            .get_thread(second_id)
            .await
            .expect("get second thread")
            .is_some()
    );

    let state = runtime
        .backfill_coordinator()
        .state()
        .await
        .expect("get backfill state");
    assert_eq!(state.status, BackfillStatus::Complete);
    assert_eq!(
        state.last_watermark,
        Some(backfill_watermark_for_path(
            codex_home.as_path(),
            second_path.as_path()
        ))
    );
    assert!(state.last_success_at.is_some());
}

#[tokio::test]
async fn backfill_runner_skips_busy_owner_then_takes_over_expired_lease() {
    let dir = tempdir().expect("tempdir");
    let codex_home = dir.path().to_path_buf();
    let thread_uuid = Uuid::new_v4();
    write_rollout_in_sessions(
        codex_home.as_path(),
        "2026-01-27T12-34-56",
        "2026-01-27T12:34:56Z",
        thread_uuid,
        /*git*/ None,
    );
    let first =
        codex_state::StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize first runtime");
    let second =
        codex_state::StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize second runtime");
    let coordinator = first.backfill_coordinator();
    let active_lease = match coordinator
        .try_claim("active-worker", Duration::from_secs(60))
        .await
        .expect("claim active lease")
    {
        BackfillClaimOutcome::Claimed { lease, .. } => lease,
        outcome => panic!("expected active lease, got {outcome:?}"),
    };

    let busy_outcome = backfill_sessions_with_lease(
        second.as_ref(),
        codex_home.as_path(),
        "test-provider",
        Duration::from_secs(60),
        "contending-worker",
    )
    .await;
    assert_eq!(busy_outcome, BackfillRunOutcome::Busy);
    let thread_id = ThreadId::from_string(&thread_uuid.to_string()).expect("thread id");
    assert_eq!(
        second.get_thread(thread_id).await.expect("get thread"),
        None
    );

    assert_eq!(
        coordinator
            .release(&active_lease)
            .await
            .expect("release lease"),
        BackfillLeaseUpdate::Applied
    );
    let expired_lease = match coordinator
        .try_claim("failed-worker", Duration::ZERO)
        .await
        .expect("claim expiring lease")
    {
        BackfillClaimOutcome::Claimed { lease, .. } => lease,
        outcome => panic!("expected expiring lease, got {outcome:?}"),
    };

    let retry_outcome = backfill_sessions_with_lease(
        second.as_ref(),
        codex_home.as_path(),
        "test-provider",
        Duration::from_secs(60),
        "retry-worker",
    )
    .await;
    assert_eq!(retry_outcome, BackfillRunOutcome::Completed);

    assert!(
        second
            .get_thread(thread_id)
            .await
            .expect("get thread")
            .is_some()
    );
    assert_eq!(
        second
            .backfill_coordinator()
            .state()
            .await
            .expect("get backfill state")
            .status,
        BackfillStatus::Complete
    );
    assert_eq!(
        coordinator
            .complete(&expired_lease, /*last_watermark*/ None)
            .await
            .expect("reject stale completion"),
        BackfillLeaseUpdate::Rejected
    );
    assert_eq!(
        backfill_sessions_with_lease(
            second.as_ref(),
            codex_home.as_path(),
            "test-provider",
            Duration::from_secs(60),
            "late-worker",
        )
        .await,
        BackfillRunOutcome::AlreadyComplete
    );
}

#[tokio::test]
async fn backfill_runner_heartbeats_while_processing_a_long_batch() {
    let dir = tempdir().expect("tempdir");
    let codex_home = dir.path().to_path_buf();
    let mut last_thread_id = None;
    for _ in 0..=BACKFILL_BATCH_SIZE {
        let thread_uuid = Uuid::new_v4();
        write_rollout_in_sessions(
            codex_home.as_path(),
            "2026-01-27T12-34-56",
            "2026-01-27T12:34:56Z",
            thread_uuid,
            /*git*/ None,
        );
        last_thread_id = Some(ThreadId::from_string(&thread_uuid.to_string()).expect("thread id"));
    }
    let runtime =
        codex_state::StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");
    let second =
        codex_state::StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize second runtime");

    let (first_outcome, second_outcome) = tokio::join!(
        backfill_sessions_with_lease(
            runtime.as_ref(),
            codex_home.as_path(),
            "test-provider",
            Duration::from_millis(100),
            "heartbeat-worker-a",
        ),
        backfill_sessions_with_lease(
            second.as_ref(),
            codex_home.as_path(),
            "test-provider",
            Duration::from_millis(100),
            "heartbeat-worker-b",
        )
    );
    assert!(
        matches!(
            (first_outcome, second_outcome),
            (BackfillRunOutcome::Completed, BackfillRunOutcome::Busy)
                | (BackfillRunOutcome::Busy, BackfillRunOutcome::Completed)
        ),
        "exactly one concurrent runner should execute the backfill"
    );

    assert_eq!(
        runtime
            .backfill_coordinator()
            .state()
            .await
            .expect("get backfill state")
            .status,
        BackfillStatus::Complete
    );
    assert!(
        runtime
            .get_thread(last_thread_id.expect("last thread id"))
            .await
            .expect("get last thread")
            .is_some()
    );
}

#[tokio::test]
async fn cancelled_backfill_runner_releases_lease_for_retry() {
    let dir = tempdir().expect("tempdir");
    let codex_home = dir.path().to_path_buf();
    let mut last_thread_id = None;
    for _ in 0..300 {
        let thread_uuid = Uuid::new_v4();
        write_rollout_in_sessions(
            codex_home.as_path(),
            "2026-01-27T12-34-56",
            "2026-01-27T12:34:56Z",
            thread_uuid,
            /*git*/ None,
        );
        last_thread_id = Some(ThreadId::from_string(&thread_uuid.to_string()).expect("thread id"));
    }
    let first =
        codex_state::StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize first runtime");
    let second =
        codex_state::StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize second runtime");
    let first_runtime = first.clone();
    let first_home = codex_home.clone();
    let runner = tokio::spawn(async move {
        backfill_sessions_with_lease(
            first_runtime.as_ref(),
            first_home.as_path(),
            "test-provider",
            Duration::from_secs(60),
            "cancelled-worker",
        )
        .await;
    });
    let coordinator = second.backfill_coordinator();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let state = coordinator.state().await.expect("get running state");
            match state.status {
                BackfillStatus::Running if state.last_watermark.is_some() => break,
                BackfillStatus::Pending | BackfillStatus::Running => {
                    tokio::task::yield_now().await;
                }
                BackfillStatus::Complete => panic!("backfill completed before cancellation"),
            }
        }
    })
    .await
    .expect("runner should checkpoint its lease");

    runner.abort();
    assert!(
        runner
            .await
            .expect_err("runner should be cancelled")
            .is_cancelled()
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if coordinator
                .state()
                .await
                .expect("get released state")
                .status
                == BackfillStatus::Pending
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled runner should release its lease");

    let retry_outcome = backfill_sessions_with_lease(
        second.as_ref(),
        codex_home.as_path(),
        "test-provider",
        Duration::from_secs(60),
        "retry-worker",
    )
    .await;
    assert_eq!(retry_outcome, BackfillRunOutcome::Completed);

    assert_eq!(
        coordinator
            .state()
            .await
            .expect("get complete state")
            .status,
        BackfillStatus::Complete
    );
    assert!(
        second
            .get_thread(last_thread_id.expect("last thread id"))
            .await
            .expect("get last thread")
            .is_some()
    );
}

#[tokio::test]
async fn backfill_sessions_preserves_existing_git_branch_and_fills_missing_git_fields() {
    let dir = tempdir().expect("tempdir");
    let codex_home = dir.path().to_path_buf();
    let thread_uuid = Uuid::new_v4();
    let rollout_path = write_rollout_in_sessions(
        codex_home.as_path(),
        "2026-01-27T12-34-56",
        "2026-01-27T12:34:56Z",
        thread_uuid,
        Some(GitInfo {
            commit_hash: Some(codex_git_utils::GitSha::new("rollout-sha")),
            branch: Some("rollout-branch".to_string()),
            repository_url: Some(
                SanitizedGitUrl::try_from("git@example.com:openai/codex.git")
                    .expect("valid git remote URL"),
            ),
        }),
    );

    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("initialize runtime");
    let thread_id = ThreadId::from_string(&thread_uuid.to_string()).expect("thread id");
    let mut existing = extract_metadata_from_rollout(&rollout_path, "test-provider")
        .await
        .expect("extract")
        .metadata;
    existing.git_sha = None;
    existing.git_branch = Some("sqlite-branch".to_string());
    existing.git_origin_url = None;
    runtime
        .upsert_thread(&existing)
        .await
        .expect("existing metadata upsert");

    backfill_sessions_with_owner(
        runtime.as_ref(),
        codex_home.as_path(),
        "test-provider",
        "test-owner",
    )
    .await;

    let persisted = runtime
        .get_thread(thread_id)
        .await
        .expect("get thread")
        .expect("thread exists");
    assert_eq!(persisted.git_sha.as_deref(), Some("rollout-sha"));
    assert_eq!(persisted.git_branch.as_deref(), Some("sqlite-branch"));
    assert_eq!(
        persisted.git_origin_url.as_deref(),
        Some("git@example.com:openai/codex.git")
    );
}

#[tokio::test]
async fn backfill_sessions_preserves_existing_paginated_memory_mode() {
    let dir = tempdir().expect("tempdir");
    let codex_home = dir.path().to_path_buf();
    let thread_uuid = Uuid::new_v4();
    let rollout_path = write_rollout_in_sessions_with_cwd(
        codex_home.as_path(),
        "2026-01-27T12-34-56",
        "2026-01-27T12:34:56Z",
        thread_uuid,
        codex_home.clone(),
        /*git*/ None,
        ThreadHistoryMode::Paginated,
    );

    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("initialize runtime");
    let thread_id = ThreadId::from_string(&thread_uuid.to_string()).expect("thread id");
    let existing = extract_metadata_from_rollout(&rollout_path, "test-provider")
        .await
        .expect("extract")
        .metadata;
    runtime
        .upsert_thread(&existing)
        .await
        .expect("existing metadata upsert");
    assert!(
        runtime
            .set_thread_memory_mode(thread_id, "disabled")
            .await
            .expect("disable memory mode")
    );

    backfill_sessions_with_owner(
        runtime.as_ref(),
        codex_home.as_path(),
        "test-provider",
        "test-owner",
    )
    .await;

    assert_eq!(
        runtime
            .get_thread_memory_mode(thread_id)
            .await
            .expect("get memory mode")
            .as_deref(),
        Some("disabled")
    );
}

#[tokio::test]
async fn backfill_sessions_normalizes_cwd_before_upsert() {
    let dir = tempdir().expect("tempdir");
    let codex_home = dir.path().to_path_buf();
    let thread_uuid = Uuid::new_v4();
    let session_cwd = codex_home.join(".");
    let rollout_path = write_rollout_in_sessions_with_cwd(
        codex_home.as_path(),
        "2026-01-27T12-34-56",
        "2026-01-27T12:34:56Z",
        thread_uuid,
        session_cwd.clone(),
        /*git*/ None,
        ThreadHistoryMode::Legacy,
    );

    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("initialize runtime");

    backfill_sessions_with_owner(
        runtime.as_ref(),
        codex_home.as_path(),
        "test-provider",
        "test-owner",
    )
    .await;

    let thread_id = ThreadId::from_string(&thread_uuid.to_string()).expect("thread id");
    let stored = runtime
        .get_thread(thread_id)
        .await
        .expect("get thread")
        .expect("thread should be backfilled");

    assert_eq!(stored.rollout_path, rollout_path);
    assert_eq!(stored.cwd, normalize_cwd_for_state_db(&session_cwd));
}

fn write_rollout_in_sessions(
    codex_home: &Path,
    filename_ts: &str,
    event_ts: &str,
    thread_uuid: Uuid,
    git: Option<GitInfo>,
) -> PathBuf {
    write_rollout_in_sessions_with_cwd(
        codex_home,
        filename_ts,
        event_ts,
        thread_uuid,
        codex_home.to_path_buf(),
        git,
        ThreadHistoryMode::Legacy,
    )
}

fn write_rollout_in_sessions_with_cwd(
    codex_home: &Path,
    filename_ts: &str,
    event_ts: &str,
    thread_uuid: Uuid,
    cwd: PathBuf,
    git: Option<GitInfo>,
    history_mode: ThreadHistoryMode,
) -> PathBuf {
    let id = ThreadId::from_string(&thread_uuid.to_string()).expect("thread id");
    let sessions_dir = codex_home.join("sessions");
    std::fs::create_dir_all(sessions_dir.as_path()).expect("create sessions dir");
    let path = sessions_dir.join(format!("rollout-{filename_ts}-{thread_uuid}.jsonl"));
    let session_meta = SessionMeta {
        session_id: id.into(),
        id,
        forked_from_id: None,
        forked_from_ordinal_exclusive: None,
        parent_thread_id: None,
        timestamp: event_ts.to_string(),
        cwd,
        originator: "cli".to_string(),
        cli_version: "0.0.0".to_string(),
        source: SessionSource::default(),
        thread_source: None,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
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
    };
    let session_meta_line = SessionMetaLine {
        meta: session_meta,
        git,
    };
    let rollout_line = RolloutLine {
        timestamp: event_ts.to_string(),
        ordinal: None,
        item: RolloutItem::SessionMeta(session_meta_line),
    };
    let json = serde_json::to_string(&rollout_line).expect("serialize rollout");
    let mut file = File::create(&path).expect("create rollout");
    writeln!(file, "{json}").expect("write rollout");
    path
}
