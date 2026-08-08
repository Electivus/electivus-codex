use super::MemoryArtifact;
use super::MemoryArtifactSet;
use super::MemoryStore;
use super::MemoryWorkspaceMaterialization;
use super::StateRuntime;
use super::memory_store_phase2_success_contract_tests::persist_stage1_output;
use super::test_support::test_thread_metadata;
use super::test_support::unique_temp_dir;
use crate::Phase2JobClaimOutcome;
use crate::Stage1JobClaimOutcome;
use anyhow::Result;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use sqlx::Row;
use std::future::Future;
use std::sync::Arc;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryResetSnapshot {
    pub(crate) memory_jobs: i64,
    pub(crate) outputs: i64,
    pub(crate) used_outputs: i64,
    pub(crate) selected_outputs: i64,
    pub(crate) generations: i64,
    pub(crate) artifacts: i64,
    pub(crate) active_generations: i64,
    pub(crate) threads: i64,
    pub(crate) history_items: i64,
    pub(crate) disabled_modes: i64,
    pub(crate) pollution_overrides: i64,
}

pub(crate) async fn run_memory_reset_contract<Age, AgeFuture, Snapshot, SnapshotFuture>(
    writer: &MemoryStore,
    resetter: &MemoryStore,
    output_thread_id: ThreadId,
    age_phase2_success: Age,
    mut snapshot: Snapshot,
    expected_seeded: MemoryResetSnapshot,
    expected_materialization: MemoryWorkspaceMaterialization,
) -> Result<()>
where
    Age: FnOnce() -> AgeFuture,
    AgeFuture: Future<Output = Result<()>>,
    Snapshot: FnMut() -> SnapshotFuture,
    SnapshotFuture: Future<Output = Result<MemoryResetSnapshot>>,
{
    writer.clear_memory_data().await?;
    let source_updated_at = 1_700_000_000;
    persist_stage1_output(writer, output_thread_id, source_updated_at, "reset").await?;
    assert_eq!(
        writer
            .record_stage1_output_usage(&[output_thread_id])
            .await?,
        1
    );
    let selected_outputs = writer
        .get_phase2_input_selection(/*n*/ 1, /*max_unused_days*/ 36_500)
        .await?;
    assert_eq!(selected_outputs.len(), 1);
    let (phase2_token, input_watermark) = claim_phase2(writer).await?;
    assert_eq!(input_watermark, source_updated_at);
    let artifacts = MemoryArtifactSet::new(vec![MemoryArtifact::new(
        "MEMORY.md",
        b"authoritative memory before reset\n".to_vec(),
    )?])?;
    assert!(
        writer
            .complete_global_consolidation(
                &phase2_token,
                source_updated_at,
                &selected_outputs,
                &artifacts,
            )
            .await?
    );
    assert!(
        writer
            .mark_thread_memory_mode_polluted(output_thread_id)
            .await?
    );

    let stale_stage1_token = match writer
        .try_claim_stage1_job(
            output_thread_id,
            ThreadId::new(),
            source_updated_at + 1,
            /*lease_seconds*/ 60,
            /*max_running_jobs*/ 4,
        )
        .await?
    {
        Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
        outcome @ (Stage1JobClaimOutcome::SkippedUpToDate
        | Stage1JobClaimOutcome::SkippedRunning
        | Stage1JobClaimOutcome::SkippedRetryBackoff
        | Stage1JobClaimOutcome::SkippedRetryExhausted) => {
            anyhow::bail!("expected stage-one claim before reset, got {outcome:?}")
        }
    };
    age_phase2_success().await?;
    let (stale_phase2_token, _) = claim_phase2(writer).await?;
    assert_eq!(snapshot().await?, expected_seeded);

    resetter.clear_memory_data().await?;

    assert!(
        !writer
            .mark_stage1_job_succeeded(
                output_thread_id,
                &stale_stage1_token,
                source_updated_at + 1,
                "stale raw memory",
                "stale rollout summary",
                Some("stale-reset"),
            )
            .await?
    );
    assert!(
        !writer
            .complete_global_consolidation(
                &stale_phase2_token,
                source_updated_at + 1,
                &selected_outputs,
                &MemoryArtifactSet::new(vec![MemoryArtifact::new(
                    "MEMORY.md",
                    b"stale generation\n".to_vec(),
                )?])?,
            )
            .await?
    );
    assert_eq!(
        snapshot().await?,
        MemoryResetSnapshot {
            memory_jobs: 0,
            outputs: 0,
            used_outputs: 0,
            selected_outputs: 0,
            generations: 0,
            artifacts: 0,
            active_generations: 0,
            threads: expected_seeded.threads,
            history_items: expected_seeded.history_items,
            disabled_modes: expected_seeded.disabled_modes,
            pollution_overrides: expected_seeded.pollution_overrides,
        }
    );
    assert_eq!(
        resetter.memory_workspace_materialization().await?,
        expected_materialization
    );
    assert_eq!(resetter.load_active_memory_generation().await?, None);
    Ok(())
}

async fn claim_phase2(store: &MemoryStore) -> Result<(String, i64)> {
    match store
        .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
        .await?
    {
        Phase2JobClaimOutcome::Claimed {
            ownership_token,
            input_watermark,
        } => Ok((ownership_token, input_watermark)),
        outcome @ (Phase2JobClaimOutcome::SkippedRetryUnavailable
        | Phase2JobClaimOutcome::SkippedCooldown
        | Phase2JobClaimOutcome::SkippedRunning) => {
            anyhow::bail!("expected phase-two claim, got {outcome:?}")
        }
    }
}

#[tokio::test]
async fn sqlite_memory_reset_satisfies_shared_contract() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let writer = StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string()).await?;
    let resetter =
        StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string()).await?;
    let output_thread_id = ThreadId::new();
    let disabled_thread_id = ThreadId::new();
    for thread_id in [output_thread_id, disabled_thread_id] {
        writer
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.join(format!("reset-{thread_id}")),
            ))
            .await?;
    }
    writer
        .set_thread_memory_mode(disabled_thread_id, "disabled")
        .await?;
    let memory_pool = writer.memories().sqlite_pool_for_tests().clone();
    let state_pool = Arc::clone(writer.sqlite_pool_arc().expect("SQLite runtime"));
    let age_pool = memory_pool.clone();
    let snapshot = move || {
        let memory_pool = memory_pool.clone();
        let state_pool = state_pool.clone();
        async move { sqlite_snapshot(&memory_pool, &state_pool).await }
    };

    run_memory_reset_contract(
        writer.memories(),
        resetter.memories(),
        output_thread_id,
        move || async move {
            sqlx::query("UPDATE jobs SET finished_at = 0 WHERE kind = 'memory_consolidate_global'")
                .execute(&age_pool)
                .await?;
            Ok(())
        },
        snapshot,
        MemoryResetSnapshot {
            memory_jobs: 2,
            outputs: 1,
            used_outputs: 1,
            selected_outputs: 1,
            generations: 0,
            artifacts: 0,
            active_generations: 0,
            threads: 2,
            history_items: 0,
            disabled_modes: 1,
            pollution_overrides: 1,
        },
        MemoryWorkspaceMaterialization::Preserve,
    )
    .await?;
    writer.close().await;
    resetter.close().await;
    Ok(())
}

async fn sqlite_snapshot(
    memory_pool: &sqlx::SqlitePool,
    state_pool: &sqlx::SqlitePool,
) -> Result<MemoryResetSnapshot> {
    let memory = sqlx::query(
        "SELECT (SELECT COUNT(*) FROM jobs WHERE kind IN \
         ('memory_stage1', 'memory_consolidate_global')) AS memory_jobs, \
         COUNT(*) AS outputs, COUNT(*) FILTER (WHERE usage_count > 0 AND last_usage IS NOT NULL) \
         AS used_outputs, COUNT(*) FILTER (WHERE selected_for_phase2 != 0 AND \
         selected_for_phase2_source_updated_at IS NOT NULL) AS selected_outputs \
         FROM stage1_outputs",
    )
    .fetch_one(memory_pool)
    .await?;
    let thread = sqlx::query(
        "SELECT COUNT(*) AS threads, COUNT(*) FILTER (WHERE memory_mode = 'disabled') AS \
         disabled_modes, COUNT(*) FILTER (WHERE memory_mode = 'polluted') AS pollution_overrides \
         FROM threads",
    )
    .fetch_one(state_pool)
    .await?;
    Ok(MemoryResetSnapshot {
        memory_jobs: memory.try_get("memory_jobs")?,
        outputs: memory.try_get("outputs")?,
        used_outputs: memory.try_get("used_outputs")?,
        selected_outputs: memory.try_get("selected_outputs")?,
        generations: 0,
        artifacts: 0,
        active_generations: 0,
        threads: thread.try_get("threads")?,
        history_items: 0,
        disabled_modes: thread.try_get("disabled_modes")?,
        pollution_overrides: thread.try_get("pollution_overrides")?,
    })
}
