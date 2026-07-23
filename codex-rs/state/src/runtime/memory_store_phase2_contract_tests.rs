use super::MemoryStore;
use super::StateRuntime;
use super::test_support::unique_temp_dir;
use crate::Phase2JobClaimOutcome;
use anyhow::Result;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

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

pub(crate) async fn run_phase2_enqueue_and_claim_contract(
    first: &MemoryStore,
    second: &MemoryStore,
) -> Result<()> {
    first
        .enqueue_global_consolidation(/*input_watermark*/ 500)
        .await?;
    second
        .enqueue_global_consolidation(/*input_watermark*/ 400)
        .await?;

    let first_claim = first.try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60);
    let second_claim =
        second.try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60);
    let (first_claim, second_claim) = tokio::join!(first_claim, second_claim);
    let outcomes = [first_claim?, second_claim?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| {
                matches!(
                    outcome,
                    Phase2JobClaimOutcome::Claimed {
                        input_watermark: 501,
                        ..
                    }
                )
            })
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Phase2JobClaimOutcome::SkippedRunning))
            .count(),
        1
    );
    Ok(())
}

pub(crate) async fn run_phase2_heartbeat_and_failure_contract(
    first: &MemoryStore,
    second: &MemoryStore,
) -> Result<()> {
    first.clear_memory_data().await?;
    first
        .enqueue_global_consolidation(/*input_watermark*/ 600)
        .await?;
    let initial_token = claimed_token(
        first
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
            .await?,
        /*input_watermark*/ 600,
    );
    assert!(
        !second
            .heartbeat_global_phase2_job("wrong-token", /*lease_seconds*/ 60)
            .await?
    );
    assert!(
        second
            .heartbeat_global_phase2_job(&initial_token, /*lease_seconds*/ 60)
            .await?
    );
    assert!(
        !second
            .mark_global_phase2_job_failed(
                "wrong-token",
                "must not fail another owner",
                /*retry_delay_seconds*/ 0,
            )
            .await?
    );
    assert!(
        !first
            .mark_global_phase2_job_failed_if_unowned(
                &initial_token,
                "must not fail active owner",
                /*retry_delay_seconds*/ 0,
            )
            .await?
    );
    assert!(
        first
            .heartbeat_global_phase2_job(&initial_token, /*lease_seconds*/ 0)
            .await?
    );
    assert!(
        !second
            .heartbeat_global_phase2_job(&initial_token, /*lease_seconds*/ 60)
            .await?
    );
    assert!(
        !second
            .mark_global_phase2_job_failed(
                &initial_token,
                "expired owner",
                /*retry_delay_seconds*/ 0,
            )
            .await?
    );

    let takeover_token = claimed_token(
        second
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
            .await?,
        /*input_watermark*/ 600,
    );
    assert_ne!(takeover_token, initial_token);
    assert!(
        !first
            .mark_global_phase2_job_failed_if_unowned(
                &initial_token,
                "stale owner",
                /*retry_delay_seconds*/ 0,
            )
            .await?
    );
    assert!(
        !first
            .mark_global_phase2_job_failed_if_unowned(
                &takeover_token,
                "must not fail takeover owner",
                /*retry_delay_seconds*/ 0,
            )
            .await?
    );
    assert!(
        second
            .heartbeat_global_phase2_job(&takeover_token, /*lease_seconds*/ 0)
            .await?
    );
    assert!(
        !first
            .mark_global_phase2_job_failed(
                &takeover_token,
                "expired takeover owner",
                /*retry_delay_seconds*/ 0,
            )
            .await?
    );
    assert!(
        first
            .mark_global_phase2_job_failed_if_unowned(
                &takeover_token,
                "orphaned after lease expiry",
                /*retry_delay_seconds*/ 0,
            )
            .await?
    );
    assert!(
        !second
            .mark_global_phase2_job_failed_if_unowned(
                &takeover_token,
                "must not consume retry twice",
                /*retry_delay_seconds*/ 0,
            )
            .await?
    );
    let retry_token = claimed_token(
        second
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
            .await?,
        /*input_watermark*/ 600,
    );
    assert!(
        first
            .mark_global_phase2_job_failed(
                &retry_token,
                "retry later",
                /*retry_delay_seconds*/ 60,
            )
            .await?
    );
    assert_eq!(
        second
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
            .await?,
        Phase2JobClaimOutcome::SkippedRetryUnavailable
    );

    first.clear_memory_data().await?;
    first
        .enqueue_global_consolidation(/*input_watermark*/ 700)
        .await?;
    for attempt in 0..3 {
        let token = claimed_token(
            first
                .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
                .await?,
            /*input_watermark*/ 700,
        );
        assert!(
            second
                .mark_global_phase2_job_failed(
                    &token,
                    &format!("retry failure {attempt}"),
                    /*retry_delay_seconds*/ 0,
                )
                .await?
        );
    }
    let token_after_retry_exhaustion = claimed_token(
        second
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
            .await?,
        /*input_watermark*/ 700,
    );
    assert!(
        first
            .heartbeat_global_phase2_job(&token_after_retry_exhaustion, /*lease_seconds*/ 60)
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_phase2_enqueue_and_claim_satisfies_shared_contract() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let first = StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string()).await?;
    let second = StateRuntime::init_sqlite(codex_home, "test-provider".to_string()).await?;

    run_phase2_enqueue_and_claim_contract(first.memories(), second.memories()).await?;
    run_phase2_heartbeat_and_failure_contract(first.memories(), second.memories()).await?;

    first.close().await;
    second.close().await;
    Ok(())
}
