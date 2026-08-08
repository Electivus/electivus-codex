#![allow(clippy::unwrap_used)]
use anyhow::Context;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex_exec::test_codex_exec;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::string::ToString;
use tempfile::TempDir;
use uuid::Uuid;
use walkdir::WalkDir;
use wiremock::MockServer;

/// Utility: scan the sessions dir for a rollout file that contains `marker`
/// in any response_item.message.content entry. Returns the absolute path.
fn find_session_file_containing_marker(
    sessions_dir: &std::path::Path,
    marker: &str,
) -> Option<std::path::PathBuf> {
    for entry in WalkDir::new(sessions_dir) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if !entry.file_name().to_string_lossy().ends_with(".jsonl") {
            continue;
        }
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        // Skip the first meta line and scan remaining JSONL entries.
        let mut lines = content.lines();
        if lines.next().is_none() {
            continue;
        }
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(item): Result<Value, _> = serde_json::from_str(line) else {
                continue;
            };
            if item.get("type").and_then(|t| t.as_str()) == Some("response_item")
                && let Some(payload) = item.get("payload")
                && payload.get("type").and_then(|t| t.as_str()) == Some("message")
                && payload
                    .get("content")
                    .map(ToString::to_string)
                    .unwrap_or_default()
                    .contains(marker)
            {
                return Some(path.to_path_buf());
            }
        }
    }
    None
}

/// Extract the conversation UUID from the first SessionMeta line in the rollout file.
fn extract_conversation_id(path: &std::path::Path) -> String {
    let content = std::fs::read_to_string(path).unwrap();
    let mut lines = content.lines();
    let meta_line = lines.next().expect("missing meta line");
    let meta: Value = serde_json::from_str(meta_line).expect("invalid meta json");
    meta.get("payload")
        .and_then(|p| p.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn last_user_image_count(path: &std::path::Path) -> usize {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut last_count = 0;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(item): Result<Value, _> = serde_json::from_str(line) else {
            continue;
        };
        if item.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue;
        }
        let Some(payload) = item.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        if payload.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let Some(content_items) = payload.get("content").and_then(|v| v.as_array()) else {
            continue;
        };
        last_count = content_items
            .iter()
            .filter(|entry| entry.get("type").and_then(|t| t.as_str()) == Some("input_image"))
            .count();
    }
    last_count
}

fn exec_repo_root() -> anyhow::Result<std::path::PathBuf> {
    Ok(codex_utils_cargo_bin::repo_root()?)
}

fn init_git_repo_with_origin(path: &std::path::Path, origin: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)?;
    let init = std::process::Command::new("git")
        .arg("init")
        .arg(path)
        .output()?;
    anyhow::ensure!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let remote = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["remote", "add", "origin", origin])
        .output()?;
    anyhow::ensure!(
        remote.status.success(),
        "git remote add failed: {}",
        String::from_utf8_lossy(&remote.stderr)
    );
    Ok(())
}

fn exec_sse_response(index: usize) -> String {
    let response_id = format!("resp-exec-{index}");
    let message_id = format!("msg-exec-{index}");
    responses::sse(vec![
        responses::ev_response_created(&response_id),
        responses::ev_assistant_message(&message_id, "exec response"),
        responses::ev_completed(&response_id),
    ])
}

async fn mount_exec_responses(
    server: &MockServer,
    count: usize,
) -> core_test_support::responses::ResponseMock {
    responses::mount_sse_sequence(server, (0..count).map(exec_sse_response).collect()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_last_appends_to_existing_file() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-exec-0"),
                responses::ev_assistant_message("msg-exec-0", "exec response"),
                responses::ev_completed_with_tokens("resp-exec-0", /*total_tokens*/ 7),
            ]),
            exec_sse_response(/*index*/ 1),
        ],
    )
    .await;
    let repo_root = exec_repo_root()?;

    // 1) First run: create a session with a unique marker in the content.
    let marker = format!("resume-last-{}", Uuid::new_v4());
    let prompt = format!("echo {marker}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt)
        .assert()
        .success();

    // Find the created session file containing the marker.
    let sessions_dir = test.home_path().join("sessions");
    let path = find_session_file_containing_marker(&sessions_dir, &marker)
        .expect("no session file found after first run");

    // 2) Second run: resume the most recent file with a new marker.
    let marker2 = format!("resume-last-2-{}", Uuid::new_v4());
    let prompt2 = format!("echo {marker2}");

    let output = test
        .cmd_with_server(&server)
        .env("RUST_LOG", "codex_app_server::outgoing_message=trace")
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt2)
        .arg("resume")
        .arg("--last")
        .output()
        .context("resume run should succeed")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "resume failed: {stderr}");
    assert_eq!(
        stderr
            .matches("app-server event: thread/tokenUsage/updated")
            .count(),
        1,
        "resume should not replay restored token usage: {stderr}"
    );

    // Ensure the same file was updated and contains both markers.
    let resumed_path = find_session_file_containing_marker(&sessions_dir, &marker2)
        .expect("no resumed session file containing marker2");
    assert_eq!(
        resumed_path, path,
        "resume --last should append to existing file"
    );
    let content = std::fs::read_to_string(&resumed_path)?;
    assert!(content.contains(&marker));
    assert!(content.contains(&marker2));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_last_accepts_prompt_after_flag_in_json_mode() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 2).await;
    let repo_root = exec_repo_root()?;

    // 1) First run: create a session with a unique marker in the content.
    let marker = format!("resume-last-json-{}", Uuid::new_v4());
    let prompt = format!("echo {marker}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt)
        .assert()
        .success();

    // Find the created session file containing the marker.
    let sessions_dir = test.home_path().join("sessions");
    let path = find_session_file_containing_marker(&sessions_dir, &marker)
        .expect("no session file found after first run");

    // 2) Second run: resume the most recent file and pass the prompt after --last.
    let marker2 = format!("resume-last-json-2-{}", Uuid::new_v4());
    let prompt2 = format!("echo {marker2}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg("--json")
        .arg("resume")
        .arg("--last")
        .arg(&prompt2)
        .assert()
        .success();

    let resumed_path = find_session_file_containing_marker(&sessions_dir, &marker2)
        .expect("no resumed session file containing marker2");
    assert_eq!(
        resumed_path, path,
        "resume --last should append to existing file"
    );
    let content = std::fs::read_to_string(&resumed_path)?;
    assert!(content.contains(&marker));
    assert!(content.contains(&marker2));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_last_selects_newest_thread_in_project_session_scope() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 4).await;
    let repositories = TempDir::new()?;
    let exact_checkout = repositories.path().join("exact-checkout");
    let repository_checkout = repositories.path().join("repository-checkout");
    let unrelated_checkout = repositories.path().join("unrelated-checkout");
    init_git_repo_with_origin(&exact_checkout, "https://example.com/acme/project.git")?;
    init_git_repo_with_origin(&repository_checkout, "git@example.com:acme/project.git")?;
    init_git_repo_with_origin(
        &unrelated_checkout,
        "https://example.com/acme/unrelated.git",
    )?;

    let exact_marker = format!("resume-project-exact-{}", Uuid::new_v4());
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&exact_checkout)
        .arg(format!("echo {exact_marker}"))
        .assert()
        .success();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let repository_marker = format!("resume-project-repository-{}", Uuid::new_v4());
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repository_checkout)
        .arg(format!("echo {repository_marker}"))
        .assert()
        .success();
    let sessions_dir = test.home_path().join("sessions");
    let repository_path = find_session_file_containing_marker(&sessions_dir, &repository_marker)
        .expect("no session file found for same-repository marker");
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let unrelated_marker = format!("resume-project-unrelated-{}", Uuid::new_v4());
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&unrelated_checkout)
        .arg(format!("echo {unrelated_marker}"))
        .assert()
        .success();

    let resumed_marker = format!("resume-project-selected-{}", Uuid::new_v4());
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&exact_checkout)
        .arg("resume")
        .arg("--last")
        .arg(format!("echo {resumed_marker}"))
        .assert()
        .success();

    let resumed_path = find_session_file_containing_marker(&sessions_dir, &resumed_marker)
        .expect("no resumed session file containing project-selected marker");
    assert_eq!(
        resumed_path, repository_path,
        "resume --last should choose the newest same-repository thread"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_by_name_excludes_unrelated_repository_collision() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 3).await;
    let repositories = TempDir::new()?;
    let current_checkout = repositories.path().join("current-checkout");
    let repository_checkout = repositories.path().join("repository-checkout");
    let unrelated_checkout = repositories.path().join("unrelated-checkout");
    init_git_repo_with_origin(&current_checkout, "https://example.com/acme/project.git")?;
    init_git_repo_with_origin(&repository_checkout, "git@example.com:acme/project.git")?;
    init_git_repo_with_origin(
        &unrelated_checkout,
        "https://example.com/acme/unrelated.git",
    )?;

    let repository_marker = format!("resume-name-repository-{}", Uuid::new_v4());
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repository_checkout)
        .arg(format!("echo {repository_marker}"))
        .assert()
        .success();
    let sessions_dir = test.home_path().join("sessions");
    let repository_path = find_session_file_containing_marker(&sessions_dir, &repository_marker)
        .expect("no session file found for same-repository marker");
    let repository_id =
        codex_protocol::ThreadId::from_string(&extract_conversation_id(&repository_path))?;

    let unrelated_marker = format!("resume-name-unrelated-{}", Uuid::new_v4());
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&unrelated_checkout)
        .arg(format!("echo {unrelated_marker}"))
        .assert()
        .success();
    let unrelated_path = find_session_file_containing_marker(&sessions_dir, &unrelated_marker)
        .expect("no session file found for unrelated marker");
    let unrelated_id =
        codex_protocol::ThreadId::from_string(&extract_conversation_id(&unrelated_path))?;

    let shared_name = format!("shared-project-name-{}", Uuid::new_v4());
    let mut config = codex_core::config::ConfigBuilder::default()
        .codex_home(test.home_path().to_path_buf())
        .build()
        .await?;
    config.sqlite = codex_core::config::SqliteConfig::from_sqlite_home(
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(test.home_path())?,
    );
    let state_db = codex_core::init_state_db(&config)
        .await
        .expect("exec sessions should initialize the local Runtime State Backend");
    assert!(
        state_db
            .update_thread_name(repository_id, Some(&shared_name))
            .await?
    );
    assert!(
        state_db
            .update_thread_name(unrelated_id, Some(&shared_name))
            .await?
    );
    codex_core::append_thread_name(test.home_path(), repository_id, &shared_name).await?;
    codex_core::append_thread_name(test.home_path(), unrelated_id, &shared_name).await?;

    let resumed_marker = format!("resume-name-selected-{}", Uuid::new_v4());
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&current_checkout)
        .arg("resume")
        .arg(&shared_name)
        .arg(format!("echo {resumed_marker}"))
        .assert()
        .success();

    let resumed_path = find_session_file_containing_marker(&sessions_dir, &resumed_marker)
        .expect("no resumed session file containing name-selected marker");
    assert_eq!(
        resumed_path, repository_path,
        "name lookup should choose only the same-repository collision"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_last_respects_cwd_filter_and_all_flag() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 5).await;

    let dir_a = TempDir::new()?;
    let dir_b = TempDir::new()?;

    let marker_a = format!("resume-cwd-a-{}", Uuid::new_v4());
    let prompt_a = format!("echo {marker_a}");
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(dir_a.path())
        .arg(&prompt_a)
        .assert()
        .success();

    let marker_b = format!("resume-cwd-b-{}", Uuid::new_v4());
    let prompt_b = format!("echo {marker_b}");
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(dir_b.path())
        .arg(&prompt_b)
        .assert()
        .success();

    let sessions_dir = test.home_path().join("sessions");
    let path_a = find_session_file_containing_marker(&sessions_dir, &marker_a)
        .expect("no session file found for marker_a");
    let path_b = find_session_file_containing_marker(&sessions_dir, &marker_b)
        .expect("no session file found for marker_b");

    // `updated_at` is second-granularity, so ensure the touch lands in a later second
    // than the initial session creation on fast CI (especially Windows).
    std::thread::sleep(std::time::Duration::from_millis(1100));

    // Make thread B deterministically newest according to rollout metadata.
    let session_id_b = extract_conversation_id(&path_b);
    let marker_b_touch = format!("resume-cwd-b-touch-{}", Uuid::new_v4());
    let prompt_b_touch = format!("echo {marker_b_touch}");
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(dir_b.path())
        .arg("resume")
        .arg(&session_id_b)
        .arg(&prompt_b_touch)
        .assert()
        .success();

    // `resume --last` sorts by `updated_at`, which is second-granularity. Sleep so
    // the upcoming `resume --last --all` write lands in a later second and becomes
    // deterministically newest (instead of tying and falling back to UUID order).
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let marker_b2 = format!("resume-cwd-b-2-{}", Uuid::new_v4());
    let prompt_b2 = format!("echo {marker_b2}");
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(dir_a.path())
        .arg("resume")
        .arg("--last")
        .arg("--all")
        .arg(&prompt_b2)
        .assert()
        .success();

    let resumed_path_all = find_session_file_containing_marker(&sessions_dir, &marker_b2)
        .expect("no resumed session file containing marker_b2");
    assert_eq!(
        resumed_path_all, path_b,
        "resume --last --all should pick newest session"
    );

    let marker_a2 = format!("resume-cwd-a-2-{}", Uuid::new_v4());
    let prompt_a2 = format!("echo {marker_a2}");
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(dir_a.path())
        .arg("resume")
        .arg("--last")
        .arg(&prompt_a2)
        .assert()
        .success();

    let resumed_path_cwd = find_session_file_containing_marker(&sessions_dir, &marker_a2)
        .expect("no resumed session file containing marker_a2");
    assert_eq!(
        resumed_path_cwd, path_a,
        "project lookup without a Repository Identity should fall back to exact Recorded Working Directory"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_accepts_global_flags_after_subcommand() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 2).await;

    // Seed a session.
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("echo seed-resume-session")
        .assert()
        .success();

    // Resume while passing global flags after the subcommand to ensure clap accepts them.
    let base = format!("{}/v1", server.uri());
    let base_config = format!("openai_base_url={}", serde_json::to_string(&base)?);
    test.cmd()
        .arg("resume")
        .arg("--last")
        .arg("--config")
        .arg(base_config)
        .arg("--json")
        .arg("--model")
        .arg("gpt-5.2-codex")
        .arg("--config")
        .arg("reasoning_level=xhigh")
        .arg("--dangerously-bypass-approvals-and-sandbox")
        .arg("--skip-git-repo-check")
        .arg("echo resume-with-global-flags-after-subcommand")
        .assert()
        .success();

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_includes_output_schema_in_request() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let response_mock = mount_exec_responses(&server, /*count*/ 2).await;

    let schema_contents = serde_json::json!({
        "type": "object",
        "properties": {
            "answer": { "type": "string" }
        },
        "required": ["answer"],
        "additionalProperties": false
    });
    let schema_path = test.cwd_path().join("schema.json");
    std::fs::write(&schema_path, serde_json::to_vec_pretty(&schema_contents)?)?;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("echo seed-resume-session")
        .assert()
        .success();

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("resume")
        .arg("--last")
        .arg("--json")
        .arg("--output-schema")
        .arg(&schema_path)
        .arg("echo resume-with-schema")
        .assert()
        .success();

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let payload: Value = requests[1].body_json();
    let text = payload.get("text").expect("request missing text field");
    let format = text
        .get("format")
        .expect("request missing text.format field");
    assert_eq!(
        format,
        &serde_json::json!({
            "name": "codex_output_schema",
            "type": "json_schema",
            "strict": true,
            "schema": schema_contents,
        })
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_by_id_appends_to_existing_file() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 2).await;
    let source_cwd = TempDir::new()?;
    let current_cwd = TempDir::new()?;

    // 1) First run: create a session
    let marker = format!("resume-by-id-{}", Uuid::new_v4());
    let prompt = format!("echo {marker}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(source_cwd.path())
        .arg(&prompt)
        .assert()
        .success();

    let sessions_dir = test.home_path().join("sessions");
    let path = find_session_file_containing_marker(&sessions_dir, &marker)
        .expect("no session file found after first run");
    let session_id = extract_conversation_id(&path);
    assert!(
        !session_id.is_empty(),
        "missing conversation id in meta line"
    );

    // 2) Resume by id
    let marker2 = format!("resume-by-id-2-{}", Uuid::new_v4());
    let prompt2 = format!("echo {marker2}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(current_cwd.path())
        .arg(&prompt2)
        .arg("resume")
        .arg(&session_id)
        .assert()
        .success();

    let resumed_path = find_session_file_containing_marker(&sessions_dir, &marker2)
        .expect("no resumed session file containing marker2");
    assert_eq!(
        resumed_path, path,
        "resume by explicit id should remain global"
    );
    let content = std::fs::read_to_string(&resumed_path)?;
    assert!(content.contains(&marker));
    assert!(content.contains(&marker2));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_preserves_cli_configuration_overrides() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 2).await;
    let repo_root = exec_repo_root()?;

    let marker = format!("resume-config-{}", Uuid::new_v4());
    let prompt = format!("echo {marker}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("--sandbox")
        .arg("workspace-write")
        .arg("--model")
        .arg("gpt-5.1")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt)
        .assert()
        .success();

    let sessions_dir = test.home_path().join("sessions");
    let path = find_session_file_containing_marker(&sessions_dir, &marker)
        .expect("no session file found after first run");

    let marker2 = format!("resume-config-2-{}", Uuid::new_v4());
    let prompt2 = format!("echo {marker2}");

    let output = test
        .cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("--sandbox")
        .arg("workspace-write")
        .arg("--model")
        .arg("gpt-5.1-high")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt2)
        .arg("resume")
        .arg("--last")
        .output()
        .context("resume run should succeed")?;

    assert!(output.status.success(), "resume run failed: {output:?}");

    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("model: gpt-5.1-high"),
        "stderr missing model override: {stderr}"
    );
    if cfg!(target_os = "windows") {
        assert!(
            stderr.contains("sandbox: read-only"),
            "stderr missing downgraded sandbox note: {stderr}"
        );
    } else {
        assert!(
            stderr.contains("sandbox: workspace-write"),
            "stderr missing sandbox override: {stderr}"
        );
    }

    let resumed_path = find_session_file_containing_marker(&sessions_dir, &marker2)
        .expect("no resumed session file containing marker2");
    assert_eq!(resumed_path, path, "resume should append to same file");

    let content = std::fs::read_to_string(&resumed_path)?;
    assert!(content.contains(&marker));
    assert!(content.contains(&marker2));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_accepts_images_after_subcommand() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 2).await;
    let repo_root = exec_repo_root()?;

    let marker = format!("resume-image-{}", Uuid::new_v4());
    let prompt = format!("echo {marker}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt)
        .assert()
        .success();

    let image_path = test.cwd_path().join("resume_image.png");
    let image_path_2 = test.cwd_path().join("resume_image_2.png");
    let image_bytes: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    std::fs::write(&image_path, image_bytes)?;
    std::fs::write(&image_path_2, image_bytes)?;

    let marker2 = format!("resume-image-2-{}", Uuid::new_v4());
    let prompt2 = format!("echo {marker2}");
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg("resume")
        .arg("--last")
        .arg("--image")
        .arg(&image_path)
        .arg("--image")
        .arg(&image_path_2)
        .arg(&prompt2)
        .assert()
        .success();

    let sessions_dir = test.home_path().join("sessions");
    let resumed_path = find_session_file_containing_marker(&sessions_dir, &marker2)
        .expect("no session file found after resume with images");
    let image_count = last_user_image_count(&resumed_path);
    assert_eq!(
        image_count, 2,
        "resume prompt should include both attached images"
    );

    Ok(())
}
