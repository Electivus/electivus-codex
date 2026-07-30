use super::read_origin;
use codex_exec_server::ExecBackend;
use codex_exec_server::ExecBackendFuture;
use codex_exec_server::ExecParams;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::watch;

struct TimeoutExecBackend {
    process: Arc<TimeoutExecProcess>,
}

struct PendingStartExecBackend;

impl ExecBackend for PendingStartExecBackend {
    fn start(&self, _params: ExecParams) -> ExecBackendFuture<'_> {
        Box::pin(std::future::pending())
    }
}

impl ExecBackend for TimeoutExecBackend {
    fn start(&self, _params: ExecParams) -> ExecBackendFuture<'_> {
        let process = Arc::clone(&self.process);
        Box::pin(async move { Ok(StartedExecProcess { process }) })
    }
}

struct TimeoutExecProcess {
    process_id: ProcessId,
    terminate_requested: AtomicBool,
}

impl ExecProcess for TimeoutExecProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        watch::channel(0).1
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        _after_seq: Option<u64>,
        _max_bytes: Option<usize>,
        _wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(async { unreachable!("origin resolution should use process events") })
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async { unreachable!("origin resolution should not write to the process") })
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { unreachable!("origin resolution should terminate directly") })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        self.terminate_requested.store(true, Ordering::Release);
        Box::pin(std::future::pending())
    }
}

fn current_cwd_uri() -> PathUri {
    PathUri::try_from(LegacyAppPathString::from_string(
        std::env::current_dir()
            .expect("current directory")
            .to_string_lossy(),
    ))
    .expect("current directory should convert to a path URI")
}

#[tokio::test(start_paused = true)]
async fn origin_read_timeout_includes_process_startup() {
    let result = tokio::time::timeout(
        Duration::from_secs(6),
        read_origin(Arc::new(PendingStartExecBackend), current_cwd_uri()),
    )
    .await
    .expect("origin resolution must bound process startup");

    assert_eq!(result, None);
}

#[tokio::test(start_paused = true)]
async fn origin_read_timeout_requests_bounded_process_termination() {
    let process = Arc::new(TimeoutExecProcess {
        process_id: ProcessId::from("project-scope-timeout-test"),
        terminate_requested: AtomicBool::new(false),
    });
    let backend: Arc<dyn ExecBackend> = Arc::new(TimeoutExecBackend {
        process: Arc::clone(&process),
    });

    let result = tokio::time::timeout(
        Duration::from_secs(7),
        read_origin(backend, current_cwd_uri()),
    )
    .await
    .expect("origin resolution and termination must remain bounded");

    assert_eq!(result, None);
    assert!(process.terminate_requested.load(Ordering::Acquire));
}
