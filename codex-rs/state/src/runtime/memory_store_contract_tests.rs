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
                /*rollout_slug*/ None,
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

fn claimed_token(outcome: Stage1JobClaimOutcome) -> String {
    match outcome {
        Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
        Stage1JobClaimOutcome::SkippedUpToDate
        | Stage1JobClaimOutcome::SkippedRunning
        | Stage1JobClaimOutcome::SkippedRetryBackoff
        | Stage1JobClaimOutcome::SkippedRetryExhausted => {
            panic!("expected a claimed stage-one job, got {outcome:?}")
        }
    }
}

pub(crate) async fn run_stage1_retry_and_lease_contract(
    first: &MemoryStore,
    second: &MemoryStore,
    thread_id: ThreadId,
) -> Result<()> {
    let first_worker_id = ThreadId::new();
    let second_worker_id = ThreadId::new();
    let source_updated_at = 1_700_000_001;
    let first_token = claimed_token(
        first
            .try_claim_stage1_job(
                thread_id,
                first_worker_id,
                source_updated_at,
                /*lease_seconds*/ 60,
                /*max_running_jobs*/ 4,
            )
            .await?,
    );
    assert!(
        !first
            .heartbeat_stage1_job(thread_id, "wrong-token", /*lease_seconds*/ 60)
            .await?
    );
    assert!(
        first
            .heartbeat_stage1_job(thread_id, &first_token, /*lease_seconds*/ 60)
            .await?
    );
    assert_eq!(
        second
            .try_claim_stage1_job(
                thread_id,
                second_worker_id,
                source_updated_at,
                /*lease_seconds*/ 60,
                /*max_running_jobs*/ 4,
            )
            .await?,
        Stage1JobClaimOutcome::SkippedRunning
    );
    assert!(
        first
            .heartbeat_stage1_job(thread_id, &first_token, /*lease_seconds*/ 0)
            .await?
    );
    assert!(
        !first
            .heartbeat_stage1_job(thread_id, &first_token, /*lease_seconds*/ 60)
            .await?
    );
    let outputs_before_expired_finalization =
        first.list_stage1_outputs_for_global(/*n*/ 10).await?;
    assert!(
        !first
            .mark_stage1_job_succeeded(
                thread_id,
                &first_token,
                source_updated_at,
                "expired memory",
                "expired summary",
                /*rollout_slug*/ None,
            )
            .await?
    );
    assert!(
        !first
            .mark_stage1_job_succeeded_no_output(thread_id, &first_token)
            .await?
    );
    assert!(
        !first
            .mark_stage1_job_failed(
                thread_id,
                &first_token,
                "expired failure",
                /*retry_delay_seconds*/ 60,
            )
            .await?
    );
    assert_eq!(
        second.list_stage1_outputs_for_global(/*n*/ 10).await?,
        outputs_before_expired_finalization
    );
    let takeover_token = claimed_token(
        second
            .try_claim_stage1_job(
                thread_id,
                second_worker_id,
                source_updated_at,
                /*lease_seconds*/ 60,
                /*max_running_jobs*/ 4,
            )
            .await?,
    );
    assert!(
        !first
            .heartbeat_stage1_job(thread_id, &first_token, /*lease_seconds*/ 60)
            .await?
    );
    assert!(
        !first
            .mark_stage1_job_failed(
                thread_id,
                &first_token,
                "stale failure",
                /*retry_delay_seconds*/ 0,
            )
            .await?
    );
    assert!(
        !first
            .mark_stage1_job_succeeded_no_output(thread_id, &first_token)
            .await?
    );
    assert!(
        !first
            .mark_stage1_job_succeeded(
                thread_id,
                &first_token,
                source_updated_at,
                "stale memory",
                "stale summary",
                /*rollout_slug*/ None,
            )
            .await?
    );

    assert!(
        second
            .mark_stage1_job_failed(
                thread_id,
                &takeover_token,
                "retry later",
                /*retry_delay_seconds*/ 60,
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
        Stage1JobClaimOutcome::SkippedRetryBackoff
    );

    let retry_source_updated_at = source_updated_at + 1;
    for _ in 0..3 {
        let token = claimed_token(
            first
                .try_claim_stage1_job(
                    thread_id,
                    first_worker_id,
                    retry_source_updated_at,
                    /*lease_seconds*/ 60,
                    /*max_running_jobs*/ 4,
                )
                .await?,
        );
        assert!(
            second
                .mark_stage1_job_failed(
                    thread_id,
                    &token,
                    "consume retry",
                    /*retry_delay_seconds*/ 0,
                )
                .await?
        );
    }
    assert_eq!(
        second
            .try_claim_stage1_job(
                thread_id,
                second_worker_id,
                retry_source_updated_at,
                /*lease_seconds*/ 60,
                /*max_running_jobs*/ 4,
            )
            .await?,
        Stage1JobClaimOutcome::SkippedRetryExhausted
    );

    let no_output_source_updated_at = retry_source_updated_at + 1;
    let no_output_token = claimed_token(
        second
            .try_claim_stage1_job(
                thread_id,
                second_worker_id,
                no_output_source_updated_at,
                /*lease_seconds*/ 60,
                /*max_running_jobs*/ 4,
            )
            .await?,
    );
    assert!(
        first
            .mark_stage1_job_succeeded_no_output(thread_id, &no_output_token)
            .await?
    );
    assert_eq!(
        first
            .try_claim_stage1_job(
                thread_id,
                first_worker_id,
                no_output_source_updated_at,
                /*lease_seconds*/ 60,
                /*max_running_jobs*/ 4,
            )
            .await?,
        Stage1JobClaimOutcome::SkippedUpToDate
    );

    let restored_source_updated_at = no_output_source_updated_at + 1;
    let restored_token = claimed_token(
        first
            .try_claim_stage1_job(
                thread_id,
                first_worker_id,
                restored_source_updated_at,
                /*lease_seconds*/ 60,
                /*max_running_jobs*/ 4,
            )
            .await?,
    );
    assert!(
        second
            .mark_stage1_job_succeeded(
                thread_id,
                &restored_token,
                restored_source_updated_at,
                "restored durable memory",
                "restored durable summary",
                Some("restored-stage-one"),
            )
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_stage1_claim_and_output_satisfies_shared_contract() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let first = StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string()).await?;
    let second = StateRuntime::init_sqlite(codex_home, "test-provider".to_string()).await?;
    let thread_id = ThreadId::new();
    first
        .upsert_thread(&test_thread_metadata(
            first.sqlite().home(),
            thread_id,
            first.sqlite().home().join("workspace"),
        ))
        .await?;

    run_stage1_claim_and_output_contract(first.memories(), second.memories(), thread_id).await?;
    run_stage1_retry_and_lease_contract(first.memories(), second.memories(), thread_id).await?;

    first.close().await;
    second.close().await;
    Ok(())
}
