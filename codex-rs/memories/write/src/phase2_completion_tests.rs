use super::complete_phase2;
use anyhow::Result;
use codex_protocol::ThreadId;
use codex_state::MemoryStore;
use codex_state::Phase2JobClaimOutcome;
use codex_state::StateRuntime;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

async fn claim_global_job(store: &MemoryStore) -> Result<String> {
    let outcome = store
        .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
        .await?;
    match outcome {
        Phase2JobClaimOutcome::Claimed {
            ownership_token, ..
        } => Ok(ownership_token),
        Phase2JobClaimOutcome::SkippedRetryUnavailable
        | Phase2JobClaimOutcome::SkippedCooldown
        | Phase2JobClaimOutcome::SkippedRunning => {
            anyhow::bail!("expected a claimed phase-two job, got {outcome:?}")
        }
    }
}

#[tokio::test]
async fn phase2_completion_preserves_sqlite_filesystem_authority() -> Result<()> {
    let home = TempDir::new()?;
    let runtime =
        StateRuntime::init_sqlite(home.path().to_path_buf(), "test-provider".to_string()).await?;
    let memory_root = home.path().join("memories");
    tokio::fs::create_dir(&memory_root).await?;
    tokio::fs::write(memory_root.join("MEMORY.md"), "# Memory\n").await?;
    let ownership_token = claim_global_job(runtime.memories()).await?;

    assert!(
        complete_phase2(
            runtime.memories(),
            &ownership_token,
            /*completed_watermark*/ 0,
            &[],
            &memory_root,
        )
        .await?
    );
    assert_eq!(
        runtime.memories().load_active_memory_generation().await?,
        None
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn phase2_completion_path_collision_does_not_consume_job() -> Result<()> {
    let home = TempDir::new()?;
    let runtime =
        StateRuntime::init_sqlite(home.path().to_path_buf(), "test-provider".to_string()).await?;
    let memory_root = home.path().join("memories");
    tokio::fs::create_dir(&memory_root).await?;
    tokio::fs::write(memory_root.join("MEMORY.md"), "first").await?;
    tokio::fs::write(memory_root.join("memory.MD"), "second").await?;
    let ownership_token = claim_global_job(runtime.memories()).await?;

    let error = complete_phase2(
        runtime.memories(),
        &ownership_token,
        /*completed_watermark*/ 0,
        &[],
        &memory_root,
    )
    .await
    .expect_err("a case-insensitive path collision must fail artifact collection");

    assert!(error.to_string().contains("case-insensitive collision"));
    assert!(
        runtime
            .memories()
            .heartbeat_global_phase2_job(&ownership_token, /*lease_seconds*/ 60)
            .await?
    );
    Ok(())
}
