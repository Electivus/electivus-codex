use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecOutputStream;
use codex_exec_server::ExecParams;
use codex_exec_server::ExecProcessEvent;
use codex_thread_store::ThreadLocationFilter;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

const GIT_ORIGIN_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn resolve_project_location_filter(
    environment_manager: &EnvironmentManager,
    project_cwd: LegacyAppPathString,
) -> ThreadLocationFilter {
    let fallback_cwd = PathBuf::from(project_cwd.render_for_ui());
    let Ok(cwd_uri) = PathUri::try_from(project_cwd) else {
        return ThreadLocationFilter::ExactCwds(vec![fallback_cwd]);
    };
    let cwd = cwd_uri.to_path_buf();
    let Some(environment) = environment_manager.default_environment() else {
        return ThreadLocationFilter::ExactCwds(vec![cwd]);
    };
    let origin = tokio::time::timeout(
        GIT_ORIGIN_RESOLUTION_TIMEOUT,
        read_origin(environment.get_exec_backend(), cwd_uri),
    )
    .await
    .ok()
    .flatten();
    let Some(repository_identity) =
        origin.and_then(|origin| codex_git_utils::canonicalize_git_remote_url(&origin))
    else {
        return ThreadLocationFilter::ExactCwds(vec![cwd]);
    };
    ThreadLocationFilter::ProjectSessionScope {
        cwd,
        repository_identity,
    }
}

async fn read_origin(
    backend: std::sync::Arc<dyn codex_exec_server::ExecBackend>,
    cwd: PathUri,
) -> Option<String> {
    let started = backend
        .start(ExecParams {
            process_id: uuid::Uuid::new_v4().to_string().into(),
            argv: vec![
                "git".to_string(),
                "remote".to_string(),
                "get-url".to_string(),
                "origin".to_string(),
            ],
            cwd,
            env_policy: None,
            env: HashMap::new(),
            tty: false,
            pipe_stdin: false,
            arg0: None,
            sandbox: None,
            enforce_managed_network: false,
            managed_network: None,
            network_proxy: None,
        })
        .await
        .ok()?;
    let mut events = started.process.subscribe_events();
    let mut stdout = Vec::new();
    let mut succeeded = false;
    loop {
        match events.recv().await.ok()? {
            ExecProcessEvent::Output(output) if output.stream == ExecOutputStream::Stdout => {
                stdout.extend_from_slice(&output.chunk.0);
            }
            ExecProcessEvent::Output(_) => {}
            ExecProcessEvent::Exited { exit_code, .. } => succeeded = exit_code == 0,
            ExecProcessEvent::Closed { .. } => break,
            ExecProcessEvent::Failed(_) => return None,
        }
    }
    succeeded
        .then(|| String::from_utf8(stdout).ok())
        .flatten()
        .map(|origin| origin.trim().to_string())
        .filter(|origin| !origin.is_empty())
}
