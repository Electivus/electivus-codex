use super::heartbeat::HeartbeatConfig;
use super::heartbeat::HeartbeatOutcome;
use super::heartbeat::execute_with_heartbeat;
use anyhow::Result;
use codex_protocol::ThreadId;
use codex_state::Stage1JobClaimOutcome;
use codex_state::StateRuntime;
use pretty_assertions::assert_eq;
use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::time::timeout;

struct DropSignal(Option<oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

async fn sqlite_replicas() -> Result<(TempDir, Arc<StateRuntime>, Arc<StateRuntime>)> {
    let home = tempfile::tempdir()?;
    let first =
        StateRuntime::init_sqlite(home.path().to_path_buf(), "test-provider".to_string()).await?;
    let second =
        StateRuntime::init_sqlite(home.path().to_path_buf(), "test-provider".to_string()).await?;
    Ok((home, first, second))
}

async fn claim(
    runtime: &StateRuntime,
    thread_id: ThreadId,
    worker_id: ThreadId,
    lease_seconds: i64,
) -> Result<String> {
    let outcome = runtime
        .memories()
        .try_claim_stage1_job(
            thread_id,
            worker_id,
            /*source_updated_at*/ 1_700_000_000,
            lease_seconds,
            /*max_running_jobs*/ 8,
        )
        .await?;
    let Stage1JobClaimOutcome::Claimed { ownership_token } = outcome else {
        anyhow::bail!("expected claim, got {outcome:?}");
    };
    Ok(ownership_token)
}

#[tokio::test]
async fn renews_lease_while_work_remains_in_flight() -> Result<()> {
    let (_home, owner, competitor) = sqlite_replicas().await?;
    let thread_id = ThreadId::new();
    let ownership_token = claim(&owner, thread_id, ThreadId::new(), /*lease_seconds*/ 2).await?;
    let (release_sender, release_receiver) = oneshot::channel();
    let completion_called = Arc::new(AtomicBool::new(false));

    let task = tokio::spawn({
        let owner = Arc::clone(&owner);
        let completion_called = Arc::clone(&completion_called);
        let ownership_token = ownership_token.clone();
        async move {
            execute_with_heartbeat(
                owner.memories(),
                thread_id,
                &ownership_token,
                HeartbeatConfig::new(Duration::from_millis(25), /*lease_seconds*/ 2),
                async move {
                    release_receiver.await.expect("release long-running work");
                    "sampled"
                },
                move |output| async move {
                    completion_called.store(true, Ordering::SeqCst);
                    output
                },
            )
            .await
        }
    });

    tokio::time::sleep(Duration::from_millis(2_250)).await;
    assert_eq!(
        competitor
            .memories()
            .try_claim_stage1_job(
                thread_id,
                ThreadId::new(),
                /*source_updated_at*/ 1_700_000_000,
                /*lease_seconds*/ 2,
                /*max_running_jobs*/ 8,
            )
            .await?,
        Stage1JobClaimOutcome::SkippedRunning
    );
    release_sender.send(()).expect("release work");

    assert_eq!(
        timeout(Duration::from_secs(2), task).await??,
        HeartbeatOutcome::Completed("sampled")
    );
    assert!(completion_called.load(Ordering::SeqCst));

    owner.close().await;
    competitor.close().await;
    Ok(())
}

#[tokio::test]
async fn lost_ownership_cancels_work_without_running_completion() -> Result<()> {
    let (_home, owner, competitor) = sqlite_replicas().await?;
    let thread_id = ThreadId::new();
    let ownership_token = claim(
        &owner,
        thread_id,
        ThreadId::new(),
        /*lease_seconds*/ 60,
    )
    .await?;
    let (started_sender, started_receiver) = oneshot::channel();
    let (cancelled_sender, cancelled_receiver) = oneshot::channel();
    let completion_called = Arc::new(AtomicBool::new(false));

    let task = tokio::spawn({
        let owner = Arc::clone(&owner);
        let completion_called = Arc::clone(&completion_called);
        async move {
            execute_with_heartbeat(
                owner.memories(),
                thread_id,
                &ownership_token,
                HeartbeatConfig::new(Duration::from_millis(25), /*lease_seconds*/ 60),
                async move {
                    let _cancelled = DropSignal(Some(cancelled_sender));
                    started_sender.send(()).expect("signal work start");
                    pending::<&str>().await
                },
                move |output| async move {
                    completion_called.store(true, Ordering::SeqCst);
                    output
                },
            )
            .await
        }
    });

    started_receiver.await?;
    competitor.memories().clear_memory_data().await?;

    assert_eq!(
        timeout(Duration::from_secs(2), task).await??,
        HeartbeatOutcome::OwnershipLost
    );
    timeout(Duration::from_secs(1), cancelled_receiver).await??;
    assert!(!completion_called.load(Ordering::SeqCst));

    owner.close().await;
    competitor.close().await;
    Ok(())
}

#[tokio::test]
async fn failed_heartbeat_cancels_work_without_running_completion() -> Result<()> {
    let (_home, owner, competitor) = sqlite_replicas().await?;
    let thread_id = ThreadId::new();
    let ownership_token = claim(
        &owner,
        thread_id,
        ThreadId::new(),
        /*lease_seconds*/ 60,
    )
    .await?;
    let (started_sender, started_receiver) = oneshot::channel();
    let (cancelled_sender, cancelled_receiver) = oneshot::channel();
    let completion_called = Arc::new(AtomicBool::new(false));

    let task = tokio::spawn({
        let owner = Arc::clone(&owner);
        let completion_called = Arc::clone(&completion_called);
        async move {
            execute_with_heartbeat(
                owner.memories(),
                thread_id,
                &ownership_token,
                HeartbeatConfig::new(Duration::from_millis(25), /*lease_seconds*/ 60),
                async move {
                    let _cancelled = DropSignal(Some(cancelled_sender));
                    started_sender.send(()).expect("signal work start");
                    pending::<&str>().await
                },
                move |output| async move {
                    completion_called.store(true, Ordering::SeqCst);
                    output
                },
            )
            .await
        }
    });

    started_receiver.await?;
    owner.close().await;

    let outcome = timeout(Duration::from_secs(2), task).await??;
    assert!(matches!(outcome, HeartbeatOutcome::HeartbeatFailed(_)));
    timeout(Duration::from_secs(1), cancelled_receiver).await??;
    assert!(!completion_called.load(Ordering::SeqCst));

    competitor.close().await;
    Ok(())
}
