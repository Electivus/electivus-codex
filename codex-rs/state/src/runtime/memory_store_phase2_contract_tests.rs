use super::MemoryStore;
use super::StateRuntime;
use super::test_support::unique_temp_dir;
use crate::Phase2JobClaimOutcome;
use anyhow::Result;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

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

#[tokio::test]
async fn sqlite_phase2_enqueue_and_claim_satisfies_shared_contract() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let first = StateRuntime::init(codex_home.clone(), "test-provider".to_string()).await?;
    let second = StateRuntime::init(codex_home, "test-provider".to_string()).await?;

    run_phase2_enqueue_and_claim_contract(first.memories(), second.memories()).await?;

    first.close().await;
    second.close().await;
    Ok(())
}
