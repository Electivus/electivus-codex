use codex_state::BackfillCoordinator;
use codex_state::BackfillLease;
use codex_state::BackfillLeaseUpdate;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::warn;

enum Shutdown {
    Stop,
    Release,
}

/// Keeps a claimed backfill lease alive and releases it best-effort when dropped.
pub(crate) struct ActiveBackfillLease {
    coordinator: BackfillCoordinator,
    lease: BackfillLease,
    live: Arc<AtomicBool>,
    shutdown: Option<oneshot::Sender<Shutdown>>,
    heartbeat_task: Option<JoinHandle<()>>,
}

impl ActiveBackfillLease {
    pub(crate) fn new(
        coordinator: BackfillCoordinator,
        lease: BackfillLease,
        lease_duration: Duration,
    ) -> Self {
        let live = Arc::new(AtomicBool::new(true));
        let (shutdown, mut shutdown_requested) = oneshot::channel();
        let heartbeat_task = tokio::spawn({
            let coordinator = coordinator.clone();
            let lease = lease.clone();
            let live = Arc::clone(&live);
            async move {
                let heartbeat_period = std::cmp::max(
                    lease_duration / 3,
                    /*minimum heartbeat period*/ Duration::from_millis(1),
                );
                let mut heartbeat = tokio::time::interval(heartbeat_period);
                heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
                heartbeat.tick().await;
                loop {
                    tokio::select! {
                        command = &mut shutdown_requested => {
                            if !matches!(command, Ok(Shutdown::Stop))
                                && let Err(err) = coordinator.release(&lease).await
                            {
                                warn!("failed to release rollout backfill lease: {err}");
                            }
                            break;
                        }
                        _ = heartbeat.tick() => {
                            match coordinator.heartbeat(&lease, lease_duration).await {
                                Ok(BackfillLeaseUpdate::Applied) => {}
                                Ok(BackfillLeaseUpdate::Rejected) => {
                                    live.store(false, Ordering::Release);
                                    warn!("rollout backfill heartbeat lost its lease");
                                    break;
                                }
                                Err(err) => {
                                    live.store(false, Ordering::Release);
                                    warn!("rollout backfill heartbeat failed: {err}");
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        });
        Self {
            coordinator,
            lease,
            live,
            shutdown: Some(shutdown),
            heartbeat_task: Some(heartbeat_task),
        }
    }

    pub(crate) fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    pub(crate) async fn checkpoint(
        &self,
        watermark: &str,
        lease_duration: Duration,
    ) -> anyhow::Result<BackfillLeaseUpdate> {
        if !self.is_live() {
            return Ok(BackfillLeaseUpdate::Rejected);
        }
        let outcome = self
            .coordinator
            .checkpoint(&self.lease, watermark, lease_duration)
            .await;
        if !matches!(outcome, Ok(BackfillLeaseUpdate::Applied)) {
            self.live.store(false, Ordering::Release);
        }
        outcome
    }

    pub(crate) async fn heartbeat(
        &self,
        lease_duration: Duration,
    ) -> anyhow::Result<BackfillLeaseUpdate> {
        if !self.is_live() {
            return Ok(BackfillLeaseUpdate::Rejected);
        }
        let outcome = self
            .coordinator
            .heartbeat(&self.lease, lease_duration)
            .await;
        if !matches!(outcome, Ok(BackfillLeaseUpdate::Applied)) {
            self.live.store(false, Ordering::Release);
        }
        outcome
    }

    pub(crate) async fn complete(
        mut self,
        last_watermark: Option<&str>,
    ) -> anyhow::Result<BackfillLeaseUpdate> {
        let outcome = if self.is_live() {
            self.coordinator.complete(&self.lease, last_watermark).await
        } else {
            Ok(BackfillLeaseUpdate::Rejected)
        };
        let shutdown = if matches!(outcome, Ok(BackfillLeaseUpdate::Applied)) {
            Shutdown::Stop
        } else {
            Shutdown::Release
        };
        self.shutdown(shutdown).await;
        outcome
    }

    pub(crate) async fn release(mut self) {
        self.shutdown(Shutdown::Release).await;
    }

    async fn shutdown(&mut self, command: Shutdown) {
        let release_directly = self
            .shutdown
            .take()
            .is_some_and(|shutdown| matches!(shutdown.send(command), Err(Shutdown::Release)));
        if let Some(heartbeat_task) = self.heartbeat_task.take() {
            let _ = heartbeat_task.await;
        }
        if release_directly && let Err(err) = self.coordinator.release(&self.lease).await {
            warn!("failed to release rollout backfill lease: {err}");
        }
    }
}

impl Drop for ActiveBackfillLease {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(Shutdown::Release);
        }
    }
}
