use super::MemoryStore;
use super::StateRuntime;
use super::test_support::test_thread_metadata;
use super::test_support::unique_temp_dir;
use crate::MemoryArtifact;
use crate::MemoryArtifactSet;
use crate::MemoryWorkspaceMaterialization;
use crate::Phase2JobClaimOutcome;
use crate::Stage1JobClaimOutcome;
use crate::Stage1Output;
use crate::postgres::qualified_table;
use anyhow::Result;
use chrono::Utc;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use serde_json::json;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use std::future::Future;

pub(crate) fn phase2_success_thread_ids() -> Result<[ThreadId; 2]> {
    Ok([
        ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d3330001")?,
        ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d3330002")?,
    ])
}

pub(crate) async fn seed_postgres_phase2_success_threads(
    pool: &PgPool,
    schema: &str,
    thread_ids: [ThreadId; 2],
) -> Result<()> {
    let threads_table = qualified_table(schema, "threads");
    let history_table = qualified_table(schema, "thread_history");
    for (index, thread_id) in thread_ids.into_iter().enumerate() {
        let projection = json!({
            "rollout_path": format!("/contract/rollouts/phase2-success-{index}.jsonl"),
            "cwd": format!("/contract/workspaces/phase2-success-{index}"),
            "git_info": { "branch": format!("contract/phase2-success-{index}") },
        });
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {threads_table} (thread_id, projection, stream_version, fencing_token, \
             writer_id, writer_lease_expires_at, created_at, updated_at, recency_at) \
             VALUES ($1, $2, 1, 1, 'phase2-success-contract', CURRENT_TIMESTAMP, \
             CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
        )))
        .bind(thread_id.to_string())
        .bind(projection)
        .execute(pool)
        .await?;
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {history_table} (thread_id, ordinal, item) VALUES ($1, 0, $2)"
        )))
        .bind(thread_id.to_string())
        .bind(json!({
            "type": "session_meta",
            "payload": { "id": thread_id, "memory_mode": "enabled" },
        }))
        .execute(pool)
        .await?;
    }
    Ok(())
}

fn claimed_token(outcome: Phase2JobClaimOutcome, input_watermark: i64) -> String {
    match outcome {
        Phase2JobClaimOutcome::Claimed {
            ownership_token,
            input_watermark: actual_watermark,
        } => {
            assert_eq!(actual_watermark, input_watermark);
            ownership_token
        }
        Phase2JobClaimOutcome::SkippedRetryUnavailable
        | Phase2JobClaimOutcome::SkippedCooldown
        | Phase2JobClaimOutcome::SkippedRunning => {
            panic!("expected a claimed phase-two job, got {outcome:?}")
        }
    }
}

pub(crate) async fn persist_stage1_output(
    store: &MemoryStore,
    thread_id: ThreadId,
    source_updated_at: i64,
    label: &str,
) -> Result<()> {
    let outcome = store
        .try_claim_stage1_job(
            thread_id,
            ThreadId::new(),
            source_updated_at,
            /*lease_seconds*/ 60,
            /*max_running_jobs*/ 4,
        )
        .await?;
    let ownership_token = match outcome {
        Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
        Stage1JobClaimOutcome::SkippedUpToDate
        | Stage1JobClaimOutcome::SkippedRunning
        | Stage1JobClaimOutcome::SkippedRetryBackoff
        | Stage1JobClaimOutcome::SkippedRetryExhausted => {
            panic!("expected a claimed stage-one job, got {outcome:?}")
        }
    };
    assert!(
        store
            .mark_stage1_job_succeeded(
                thread_id,
                &ownership_token,
                source_updated_at,
                &format!("raw memory {label}"),
                &format!("rollout summary {label}"),
                Some(label),
            )
            .await?
    );
    Ok(())
}

fn output_for_thread(outputs: &[Stage1Output], thread_id: ThreadId) -> Stage1Output {
    outputs
        .iter()
        .find(|output| output.thread_id == thread_id)
        .unwrap_or_else(|| panic!("missing phase-two output for thread {thread_id}"))
        .clone()
}

pub(crate) async fn run_phase2_success_contract<Age, AgeFuture, Read, ReadFuture>(
    first: &MemoryStore,
    second: &MemoryStore,
    thread_ids: [ThreadId; 2],
    age_success_beyond_cooldown: Age,
    read_last_success_watermark: Read,
) -> Result<()>
where
    Age: FnOnce() -> AgeFuture,
    AgeFuture: Future<Output = Result<()>>,
    Read: FnOnce() -> ReadFuture,
    ReadFuture: Future<Output = Result<i64>>,
{
    first.clear_memory_data().await?;
    let source_a = Utc::now().timestamp().saturating_sub(2 * 60 * 60);
    let source_b = Utc::now().timestamp().saturating_sub(60 * 60);
    persist_stage1_output(first, thread_ids[0], source_a, "a-v1").await?;
    persist_stage1_output(second, thread_ids[1], source_b, "b-v1").await?;
    let initial_selection = first
        .get_phase2_input_selection(/*n*/ 2, /*max_unused_days*/ 36_500)
        .await?;
    assert_eq!(initial_selection.len(), 2);
    assert_eq!(
        second
            .get_phase2_input_selection(/*n*/ 2, /*max_unused_days*/ 36_500)
            .await?,
        initial_selection
    );
    let selected_a = output_for_thread(&initial_selection, thread_ids[0]);

    let initial_watermark = source_b;
    let initial_token = claimed_token(
        first
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
            .await?,
        initial_watermark,
    );
    assert!(
        !second
            .mark_global_phase2_job_succeeded("wrong-token", initial_watermark, &initial_selection,)
            .await?
    );
    assert!(
        first
            .heartbeat_global_phase2_job(&initial_token, /*lease_seconds*/ 0)
            .await?
    );
    assert!(
        !second
            .mark_global_phase2_job_succeeded(
                &initial_token,
                initial_watermark,
                &initial_selection,
            )
            .await?
    );
    let takeover_token = claimed_token(
        second
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
            .await?,
        initial_watermark,
    );
    assert_ne!(takeover_token, initial_token);

    persist_stage1_output(first, thread_ids[1], source_b + 1, "b-v2").await?;
    let current_success = second.mark_global_phase2_job_succeeded(
        &takeover_token,
        initial_watermark,
        &initial_selection,
    );
    let stale_success = first.mark_global_phase2_job_succeeded(
        &initial_token,
        initial_watermark,
        &initial_selection,
    );
    let (current_success, stale_success) = tokio::join!(current_success, stale_success);
    assert_eq!((current_success?, stale_success?), (true, false));
    assert_eq!(
        first
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
            .await?,
        Phase2JobClaimOutcome::SkippedCooldown
    );
    assert_eq!(
        first
            .prune_stage1_outputs_for_retention(/*max_unused_days*/ 0, /*limit*/ 100)
            .await?,
        1
    );
    assert_eq!(
        second.list_stage1_outputs_for_global(/*n*/ 10).await?,
        vec![selected_a]
    );

    persist_stage1_output(second, thread_ids[1], source_b + 2, "b-v3").await?;
    let replacement_selection = first
        .get_phase2_input_selection(/*n*/ 2, /*max_unused_days*/ 36_500)
        .await?;
    assert_eq!(replacement_selection.len(), 2);
    assert_eq!(
        second
            .get_phase2_input_selection(/*n*/ 2, /*max_unused_days*/ 36_500)
            .await?,
        replacement_selection
    );
    let replacement_b = output_for_thread(&replacement_selection, thread_ids[1]);
    age_success_beyond_cooldown().await?;
    let replacement_token = claimed_token(
        second
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
            .await?,
        source_b + 2,
    );
    let first_success = first.mark_global_phase2_job_succeeded(
        &replacement_token,
        initial_watermark - 1,
        std::slice::from_ref(&replacement_b),
    );
    let second_success = second.mark_global_phase2_job_succeeded(
        &replacement_token,
        initial_watermark - 1,
        std::slice::from_ref(&replacement_b),
    );
    let (first_success, second_success) = tokio::join!(first_success, second_success);
    let success_outcomes = [first_success?, second_success?];
    assert_eq!(
        success_outcomes
            .into_iter()
            .filter(|succeeded| *succeeded)
            .count(),
        1
    );
    assert_eq!(read_last_success_watermark().await?, initial_watermark);
    assert_eq!(
        second
            .prune_stage1_outputs_for_retention(/*max_unused_days*/ 0, /*limit*/ 100)
            .await?,
        1
    );
    assert_eq!(
        first.list_stage1_outputs_for_global(/*n*/ 10).await?,
        vec![replacement_b]
    );
    assert_eq!(
        first
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
            .await?,
        Phase2JobClaimOutcome::SkippedCooldown
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_phase2_success_satisfies_shared_contract() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let first = StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string()).await?;
    let second = StateRuntime::init_sqlite(codex_home, "test-provider".to_string()).await?;
    let thread_ids = phase2_success_thread_ids()?;
    for (index, thread_id) in thread_ids.into_iter().enumerate() {
        first
            .upsert_thread(&test_thread_metadata(
                first.sqlite().home(),
                thread_id,
                first
                    .sqlite()
                    .home()
                    .join(format!("phase2-success-{index}")),
            ))
            .await?;
    }
    let age_pool = first.memories().sqlite_pool_for_tests().clone();
    let read_pool = first.memories().sqlite_pool_for_tests().clone();
    run_phase2_success_contract(
        first.memories(),
        second.memories(),
        thread_ids,
        move || async move {
            sqlx::query(
                "UPDATE jobs SET finished_at = 0 WHERE kind = 'memory_consolidate_global' \
                 AND job_key = 'global'",
            )
            .execute(&age_pool)
            .await?;
            Ok(())
        },
        move || async move {
            Ok(sqlx::query_scalar(
                "SELECT COALESCE(last_success_watermark, -1) FROM jobs \
                 WHERE kind = 'memory_consolidate_global' AND job_key = 'global'",
            )
            .fetch_one(&read_pool)
            .await?)
        },
    )
    .await?;
    first.close().await;
    second.close().await;
    Ok(())
}

#[tokio::test]
async fn sqlite_consolidation_completion_preserves_filesystem_authority() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let runtime = StateRuntime::init_sqlite(codex_home, "test-provider".to_string()).await?;
    assert_eq!(
        runtime
            .memories()
            .memory_workspace_materialization()
            .await?,
        MemoryWorkspaceMaterialization::Preserve
    );
    runtime
        .memories()
        .enqueue_global_consolidation(/*input_watermark*/ 10)
        .await?;
    let token = claimed_token(
        runtime
            .memories()
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
            .await?,
        /*input_watermark*/ 10,
    );
    let artifacts = MemoryArtifactSet::new(vec![MemoryArtifact::new(
        "MEMORY.md",
        b"filesystem authority\n".to_vec(),
    )?])?;

    assert!(
        runtime
            .memories()
            .complete_global_consolidation(&token, /*completed_watermark*/ 10, &[], &artifacts)
            .await?
    );
    assert_eq!(
        runtime.memories().load_active_memory_generation().await?,
        None
    );
    runtime.close().await;
    Ok(())
}
