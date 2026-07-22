use super::MemoryStore;
use super::StateRuntime;
use super::test_support::test_thread_metadata;
use super::test_support::unique_temp_dir;
use crate::Stage1JobClaimOutcome;
use anyhow::Result;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

pub(crate) async fn run_stage1_claim_and_output_contract(
    first: &MemoryStore,
    second: &MemoryStore,
    thread_id: ThreadId,
) -> Result<()> {
    let first_worker_id = ThreadId::new();
    let second_worker_id = ThreadId::new();
    let source_updated_at = 1_700_000_000;
    let first_claim = first.try_claim_stage1_job(
        thread_id,
        first_worker_id,
        source_updated_at,
        /*lease_seconds*/ 60,
        /*max_running_jobs*/ 4,
    );
    let second_claim = second.try_claim_stage1_job(
        thread_id,
        second_worker_id,
        source_updated_at,
        /*lease_seconds*/ 60,
        /*max_running_jobs*/ 4,
    );
    let (first_claim, second_claim) = tokio::join!(first_claim, second_claim);
    let outcomes = [first_claim?, second_claim?];
    let ownership_token = outcomes
        .iter()
        .find_map(|outcome| match outcome {
            Stage1JobClaimOutcome::Claimed { ownership_token } => Some(ownership_token.clone()),
            Stage1JobClaimOutcome::SkippedUpToDate
            | Stage1JobClaimOutcome::SkippedRunning
            | Stage1JobClaimOutcome::SkippedRetryBackoff
            | Stage1JobClaimOutcome::SkippedRetryExhausted => None,
        })
        .expect("one replica should claim the stage-one job");
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Stage1JobClaimOutcome::Claimed { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Stage1JobClaimOutcome::SkippedRunning))
            .count(),
        1
    );
    assert_eq!(
        second
            .try_claim_stage1_job(
                thread_id,
                second_worker_id,
                source_updated_at + 1,
                /*lease_seconds*/ 60,
                /*max_running_jobs*/ 4,
            )
            .await?,
        Stage1JobClaimOutcome::SkippedRunning
    );

    assert!(
        second
            .mark_stage1_job_succeeded(
                thread_id,
                &ownership_token,
                source_updated_at,
                "durable raw memory",
                "durable rollout summary",
                Some("shared-stage-one"),
            )
            .await?
    );
    assert!(
        !first
            .mark_stage1_job_succeeded(
                thread_id,
                "stale-ownership-token",
                source_updated_at,
                "must not replace memory",
                "must not replace summary",
                None,
            )
            .await?
    );
    assert_eq!(
        first
            .try_claim_stage1_job(
                thread_id,
                first_worker_id,
                source_updated_at,
                /*lease_seconds*/ 60,
                /*max_running_jobs*/ 4,
            )
            .await?,
        Stage1JobClaimOutcome::SkippedUpToDate
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_stage1_claim_and_output_satisfies_shared_contract() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let first = StateRuntime::init(codex_home.clone(), "test-provider".to_string()).await?;
    let second = StateRuntime::init(codex_home, "test-provider".to_string()).await?;
    let thread_id = ThreadId::new();
    first
        .upsert_thread(&test_thread_metadata(
            first.codex_home(),
            thread_id,
            first.codex_home().join("workspace"),
        ))
        .await?;

    run_stage1_claim_and_output_contract(first.memories(), second.memories(), thread_id).await?;

    first.close().await;
    second.close().await;
    Ok(())
}
