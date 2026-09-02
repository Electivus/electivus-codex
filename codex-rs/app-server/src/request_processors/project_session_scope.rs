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
const GIT_PROCESS_TERMINATION_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_GIT_ORIGIN_STDOUT_BYTES: usize = 2 * 1024;

pub(super) async fn resolve_project_location_filter(
    environment_manager: &EnvironmentManager,
    project_cwd: LegacyAppPathString,
) -> ThreadLocationFilter {
    let cwd = PathBuf::from(project_cwd.as_str());
    let Some(repository_identity) =
        resolve_project_repository_identity(environment_manager, project_cwd).await
    else {
        return ThreadLocationFilter::ExactCwds(vec![cwd]);
    };
    ThreadLocationFilter::ProjectSessionScope {
        cwd,
        repository_identity,
    }
}

/// Resolves the credential-free Repository Identity for a cwd in its owning environment.
pub async fn resolve_project_repository_identity(
    environment_manager: &EnvironmentManager,
    project_cwd: LegacyAppPathString,
) -> Option<String> {
    let cwd_uri = PathUri::try_from(project_cwd).ok()?;
    let environment = environment_manager.default_environment()?;
    let origin = read_origin(environment.get_exec_backend(), cwd_uri).await?;
    codex_git_utils::canonicalize_git_remote_url(&origin)
}

async fn read_origin(
    backend: std::sync::Arc<dyn codex_exec_server::ExecBackend>,
    cwd: PathUri,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + GIT_ORIGIN_RESOLUTION_TIMEOUT;
    let started = tokio::time::timeout_at(
        deadline,
        backend.start(ExecParams {
            process_id: uuid::Uuid::new_v4().to_string().into(),
            argv: vec![
                "git".to_string(),
                "remote".to_string(),
                "get-url".to_string(),
                "origin".to_string(),
            ],
            cwd,
            env_policy: None,
            shell_snapshot: None,
            env: HashMap::new(),
            tty: false,
            pipe_stdin: false,
            arg0: None,
            sandbox: None,
            enforce_managed_network: false,
            managed_network: None,
            network_proxy: None,
        }),
    )
    .await
    .ok()?
    .ok()?;
    let process = started.process;
    let mut events = process.subscribe_events();
    let mut stdout = Vec::new();
    let mut exit_code = None;
    let mut closed = false;
    let read_result = loop {
        let event = match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(_)) | Err(_) => break Err(()),
        };
        match event {
            ExecProcessEvent::Output(output) if output.stream == ExecOutputStream::Stdout => {
                if stdout.len().saturating_add(output.chunk.0.len()) > MAX_GIT_ORIGIN_STDOUT_BYTES {
                    break Err(());
                }
                stdout.extend_from_slice(&output.chunk.0);
            }
            ExecProcessEvent::Output(_) => {}
            ExecProcessEvent::Exited {
                exit_code: process_exit_code,
                ..
            } => exit_code = Some(process_exit_code),
            ExecProcessEvent::Closed { .. } => closed = true,
            ExecProcessEvent::Failed(_) => break Err(()),
        }
        if closed && let Some(exit_code) = exit_code {
            break Ok(exit_code);
        }
    };
    let exit_code = match read_result {
        Ok(exit_code) => exit_code,
        Err(()) => {
            let _ =
                tokio::time::timeout(GIT_PROCESS_TERMINATION_TIMEOUT, process.terminate()).await;
            return None;
        }
    };
    if exit_code != 0 {
        return None;
    }
    String::from_utf8(stdout)
        .ok()
        .map(|origin| origin.trim().to_string())
        .filter(|origin| !origin.is_empty())
}

#[cfg(test)]
#[path = "project_session_scope_tests.rs"]
mod tests;
