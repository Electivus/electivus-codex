use codex_protocol::ThreadId;
use codex_state::MemoryStore;
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;
use tokio::time::MissedTickBehavior;

#[derive(Clone, Copy)]
pub(super) struct HeartbeatConfig {
    interval: Duration,
    lease_seconds: i64,
}

impl HeartbeatConfig {
    pub(super) const fn new(interval: Duration, lease_seconds: i64) -> Self {
        Self {
            interval,
            lease_seconds,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum HeartbeatOutcome<T> {
    Completed(T),
    OwnershipLost,
    HeartbeatFailed(String),
}

enum RenewalError {
    OwnershipLost,
    UpdateFailed(String),
}

impl RenewalError {
    fn into_outcome<T>(self) -> HeartbeatOutcome<T> {
        match self {
            Self::OwnershipLost => HeartbeatOutcome::OwnershipLost,
            Self::UpdateFailed(reason) => HeartbeatOutcome::HeartbeatFailed(reason),
        }
    }
}

pub(super) async fn execute_with_heartbeat<Work, WorkOutput, Complete, Completion>(
    store: &MemoryStore,
    thread_id: ThreadId,
    ownership_token: &str,
    config: HeartbeatConfig,
    work: Work,
    complete: Complete,
) -> HeartbeatOutcome<Completion::Output>
where
    Work: Future<Output = WorkOutput>,
    Complete: FnOnce(WorkOutput) -> Completion,
    Completion: Future,
{
    let first_heartbeat = Instant::now() + config.interval;
    let mut heartbeat_interval = tokio::time::interval_at(first_heartbeat, config.interval);
    heartbeat_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tokio::pin!(work);

    let work_output = loop {
        tokio::select! {
            output = &mut work => break output,
            _ = heartbeat_interval.tick() => {
                if let Err(err) = renew(store, thread_id, ownership_token, config.lease_seconds).await
                {
                    return err.into_outcome();
                }
            }
        }
    };

    if let Err(err) = renew(store, thread_id, ownership_token, config.lease_seconds).await {
        return err.into_outcome();
    }

    HeartbeatOutcome::Completed(complete(work_output).await)
}

async fn renew(
    store: &MemoryStore,
    thread_id: ThreadId,
    ownership_token: &str,
    lease_seconds: i64,
) -> Result<(), RenewalError> {
    match store
        .heartbeat_stage1_job(thread_id, ownership_token, lease_seconds)
        .await
    {
        Ok(true) => Ok(()),
        Ok(false) => Err(RenewalError::OwnershipLost),
        Err(err) => Err(RenewalError::UpdateFailed(format!("{err:#}"))),
    }
}
