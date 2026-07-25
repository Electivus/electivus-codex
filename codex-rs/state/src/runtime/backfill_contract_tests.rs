use super::BackfillClaimOutcome;
use super::BackfillCoordinator;
use super::BackfillLeaseUpdate;
use super::StateRuntime;
use super::test_support::unique_temp_dir;
use crate::BackfillState;
use crate::BackfillStatus;
use anyhow::Result;
use pretty_assertions::assert_eq;
use std::time::Duration;

const ACTIVE_LEASE: Duration = Duration::from_secs(60);

fn claimed(outcome: BackfillClaimOutcome) -> (super::BackfillLease, BackfillState) {
    match outcome {
        BackfillClaimOutcome::Claimed { lease, state } => (lease, state),
        unexpected => panic!("expected claimed backfill lease, got {unexpected:?}"),
    }
}

pub(crate) async fn run_backfill_coordination_contract(
    first: &BackfillCoordinator,
    second: &BackfillCoordinator,
) -> Result<()> {
    assert_eq!(second.state().await?, BackfillState::default());

    let (first_claim, second_claim) = tokio::join!(
        first.try_claim("worker-a", ACTIVE_LEASE),
        second.try_claim("worker-b", ACTIVE_LEASE)
    );
    let (owner, contender, first_lease, claimed_state, busy_state) =
        match (first_claim?, second_claim?) {
            (
                BackfillClaimOutcome::Claimed { lease, state },
                BackfillClaimOutcome::Busy(busy_state),
            ) => (first, second, lease, state, busy_state),
            (
                BackfillClaimOutcome::Busy(busy_state),
                BackfillClaimOutcome::Claimed { lease, state },
            ) => (second, first, lease, state, busy_state),
            unexpected => panic!("expected one claimed and one busy outcome, got {unexpected:?}"),
        };
    assert_eq!(claimed_state.status, BackfillStatus::Running);
    assert_eq!(busy_state, claimed_state);

    assert_eq!(
        owner
            .checkpoint(&first_lease, "sessions/rollout-a.jsonl", ACTIVE_LEASE)
            .await?,
        BackfillLeaseUpdate::Applied
    );
    assert_eq!(
        contender.state().await?,
        BackfillState {
            status: BackfillStatus::Running,
            last_watermark: Some("sessions/rollout-a.jsonl".to_string()),
            last_success_at: None,
        }
    );
    assert_eq!(
        owner.heartbeat(&first_lease, ACTIVE_LEASE).await?,
        BackfillLeaseUpdate::Applied
    );
    assert_eq!(
        owner.release(&first_lease).await?,
        BackfillLeaseUpdate::Applied
    );

    let (second_lease, _) = claimed(contender.try_claim("worker-next", ACTIVE_LEASE).await?);
    assert!(second_lease.fencing_token > first_lease.fencing_token);
    assert_eq!(
        owner.heartbeat(&first_lease, ACTIVE_LEASE).await?,
        BackfillLeaseUpdate::Rejected
    );
    assert_eq!(
        contender.release(&second_lease).await?,
        BackfillLeaseUpdate::Applied
    );

    let (expired_lease, _) = claimed(owner.try_claim("worker-expired", Duration::ZERO).await?);
    let (takeover_lease, _) = claimed(contender.try_claim("worker-takeover", ACTIVE_LEASE).await?);
    assert!(takeover_lease.fencing_token > expired_lease.fencing_token);
    assert_eq!(
        owner.heartbeat(&expired_lease, ACTIVE_LEASE).await?,
        BackfillLeaseUpdate::Rejected
    );
    assert_eq!(
        contender
            .complete(&takeover_lease, Some("sessions/rollout-b.jsonl"))
            .await?,
        BackfillLeaseUpdate::Applied
    );

    let complete = contender.state().await?;
    assert_eq!(complete.status, BackfillStatus::Complete);
    assert_eq!(
        complete.last_watermark,
        Some("sessions/rollout-b.jsonl".to_string())
    );
    assert!(complete.last_success_at.is_some());
    assert_eq!(
        owner.try_claim("worker-a", ACTIVE_LEASE).await?,
        BackfillClaimOutcome::Complete(complete)
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_backfill_coordination_satisfies_shared_contract() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let first = StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string()).await?;
    let second = StateRuntime::init_sqlite(codex_home, "test-provider".to_string()).await?;

    run_backfill_coordination_contract(
        &first.backfill_coordinator(),
        &second.backfill_coordinator(),
    )
    .await?;

    first.close().await;
    second.close().await;
    Ok(())
}
