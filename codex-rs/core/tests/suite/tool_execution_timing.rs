use anyhow::Context;
use anyhow::Result;
use codex_config::ToolExecutionPolicy;
use codex_config::ToolExecutionTimingRange;
use codex_features::Feature;
use codex_protocol::openai_models::ConfigShellToolType;
use core_test_support::TestTargetOs;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_sandbox;
use core_test_support::skip_if_target_windows;
use core_test_support::test_codex::test_codex;
use core_test_support::test_target_os;
use serde_json::json;

fn timing_range(min_ms: u64, default_ms: u64, max_ms: u64) -> ToolExecutionTimingRange {
    ToolExecutionTimingRange::new(min_ms, default_ms, max_ms)
        .expect("test timing range should be valid")
}

fn function_output(request: &ResponsesRequest, call_id: &str) -> Result<String> {
    let (output, _) = request
        .function_call_output_content_and_success(call_id)
        .with_context(|| format!("tool result for {call_id} should be present"))?;
    output.with_context(|| format!("tool result for {call_id} should contain text"))
}

fn reported_timeout_ms(output: &str) -> Result<u64> {
    output
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("command timed out after ")
                .and_then(|value| value.strip_suffix(" milliseconds"))
                .and_then(|value| value.parse().ok())
        })
        .with_context(|| format!("timeout duration should be present in output: {output}"))
}

fn tool_property_description(
    request: &ResponsesRequest,
    tool_name: &str,
    property_name: &str,
) -> Result<String> {
    request
        .body_json()
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .and_then(|tools| {
            tools.iter().find(|tool| {
                tool.get("name").and_then(serde_json::Value::as_str) == Some(tool_name)
            })
        })
        .and_then(|tool| tool.get("parameters"))
        .and_then(|parameters| parameters.get("properties"))
        .and_then(|properties| properties.get(property_name))
        .and_then(|property| property.get("description"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("{tool_name}.{property_name} description should be present"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_timeout_below_minimum_is_clamped_and_disclosed() -> Result<()> {
    const CALL_ID: &str = "tool-execution-timeout-min";

    skip_if_no_network!(Ok(()));

    let (command, minimum_ms) = match test_target_os() {
        TestTargetOs::Windows => (
            "Start-Sleep -Milliseconds 50; Write-Output tool-execution-complete",
            5_000,
        ),
        TestTargetOs::Linux | TestTargetOs::MacOs => {
            ("sleep 0.05; printf tool-execution-complete", 200)
        }
    };
    let arguments = serde_json::to_string(&json!({
        "command": command,
        "timeout_ms": 1,
    }))?;
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(CALL_ID, "shell_command", &arguments),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_model("gpt-5.4")
        .with_config(move |config| {
            config.tool_execution = ToolExecutionPolicy::new(
                timing_range(
                    /*min_ms*/ minimum_ms,
                    /*default_ms*/ minimum_ms + 100,
                    /*max_ms*/ minimum_ms + 300,
                ),
                config.tool_execution.yield_time(),
            );
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("run the command").await?;

    let request = response_mock
        .last_request()
        .context("model should receive the shell result")?;
    let (output, success) = request
        .function_call_output_content_and_success(CALL_ID)
        .context("shell result should be present")?;
    let output = output.context("shell result should contain text")?;
    assert_ne!(success, Some(false));
    assert!(output.contains("tool-execution-complete"), "{output}");
    assert!(
        output.contains(&format!(
            "Timing policy adjusted timeout_ms from 1 ms to {minimum_ms} ms (minimum {minimum_ms} ms)."
        )),
        "{output}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_interactive_exec_yield_below_minimum_is_clamped_and_disclosed() -> Result<()> {
    const CALL_ID: &str = "tool-execution-yield-min";
    const INTERACTIVE_CALL_ID: &str = "tool-execution-interactive-yield";

    skip_if_no_network!(Ok(()));

    let (command, yield_minimum_ms) = match test_target_os() {
        TestTargetOs::Windows => (
            "Start-Sleep -Milliseconds 50; Write-Output tool-execution-complete",
            5_000,
        ),
        TestTargetOs::Linux | TestTargetOs::MacOs => {
            ("sleep 0.05; printf tool-execution-complete", 1_000)
        }
    };
    let arguments = serde_json::to_string(&json!({
        "cmd": command,
        "tty": false,
        "yield_time_ms": 1,
    }))?;
    let interactive_arguments = serde_json::to_string(&json!({
        "cmd": command,
        "tty": true,
        "yield-time_ms": 250,
    }))?;
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(CALL_ID, "exec_command", &arguments),
                ev_function_call(INTERACTIVE_CALL_ID, "exec_command", &interactive_arguments),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_model("gpt-5.4")
        .with_config(move |config| {
            config.tool_execution = ToolExecutionPolicy::new(
                config.tool_execution.timeout(),
                timing_range(
                    /*min_ms*/ yield_minimum_ms,
                    /*default_ms*/ yield_minimum_ms + 200,
                    /*max_ms*/ yield_minimum_ms + 500,
                ),
            );
            config
                .features
                .enable(Feature::UnifiedExec)
                .expect("unified exec should be enableable");
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("run the command").await?;

    let request = response_mock
        .last_request()
        .context("model should receive the exec result")?;
    let (output, success) = request
        .function_call_output_content_and_success(CALL_ID)
        .context("exec result should be present")?;
    let output = output.context("exec result should contain text")?;
    assert_ne!(success, Some(false));
    assert!(output.contains("tool-execution-complete"), "{output}");
    assert!(
        output.contains(&format!(
            "Timing policy adjusted yield_time_ms from 1 ms to {yield_minimum_ms} ms (minimum {yield_minimum_ms} ms)."
        )),
        "{output}"
    );
    let (interactive_output, interactive_success) = request
        .function_call_output_content_and_success(INTERACTIVE_CALL_ID)
        .context("interactive exec result should be present")?;
    let interactive_output =
        interactive_output.context("interactive exec result should contain text")?;
    assert_ne!(interactive_success, Some(false));
    assert!(
        interactive_output.contains("tool-execution-complete"),
        "{interactive_output}"
    );
    assert!(
        !interactive_output.contains("Timing policy adjusted"),
        "{interactive_output}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_exec_and_wait_yields_are_clamped_and_disclosed() -> Result<()> {
    const EXEC_CALL_ID: &str = "tool-execution-code-mode-yield-min";
    const WAIT_CALL_ID: &str = "tool-execution-code-mode-wait-yield-min";

    skip_if_no_network!(Ok(()));

    let code = r#"// @exec: {"yield_time_ms": 1000}
text("tool-execution-started");
yield_control();
await new Promise((resolve) => setTimeout(resolve, 50));
text("tool-execution-complete");
"#;
    let wait_arguments = serde_json::to_string(&json!({
        "cell_id": "1",
        "yield_time_ms": 1,
    }))?;
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_custom_tool_call(EXEC_CALL_ID, "exec", code),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_function_call(WAIT_CALL_ID, "wait", &wait_arguments),
                ev_completed("resp-3"),
            ]),
            sse(vec![
                ev_response_created("resp-4"),
                ev_assistant_message("msg-2", "done"),
                ev_completed("resp-4"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex().with_model("gpt-5.4").with_config(|config| {
        config.tool_execution = ToolExecutionPolicy::new(
            timing_range(
                /*min_ms*/ 10, /*default_ms*/ 20, /*max_ms*/ 25,
            ),
            timing_range(
                /*min_ms*/ 200, /*default_ms*/ 300, /*max_ms*/ 500,
            ),
        );
        config
            .features
            .enable(Feature::CodeMode)
            .expect("code mode should be enableable");
    });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("run the code").await?;

    let request = response_mock
        .last_request()
        .context("model should receive the code-mode result")?;
    let output = request.custom_tool_call_output(EXEC_CALL_ID);
    let output = output
        .get("output")
        .context("code-mode result should contain output")?
        .to_string();
    assert!(output.contains("tool-execution-started"), "{output}");
    assert!(output.contains("Script running with cell ID 1"), "{output}");
    assert!(
        output.contains(
            "Timing policy adjusted yield_time_ms from 1000 ms to 500 ms (maximum 500 ms)."
        ),
        "{output}"
    );

    test.submit_turn("wait for the code").await?;

    let request = response_mock
        .last_request()
        .context("model should receive the code-mode wait result")?;
    let output = request.function_call_output(WAIT_CALL_ID);
    let output = output
        .get("output")
        .context("code-mode wait result should contain output")?
        .to_string();
    assert!(output.contains("tool-execution-complete"), "{output}");
    assert!(
        output
            .contains("Timing policy adjusted yield_time_ms from 1 ms to 200 ms (minimum 200 ms)."),
        "{output}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_uses_configured_default_and_clamps_to_maximum() -> Result<()> {
    const DEFAULT_CALL_ID: &str = "tool-execution-timeout-default";
    const MAX_CALL_ID: &str = "tool-execution-timeout-max";

    skip_if_no_network!(Ok(()));

    let command = match test_target_os() {
        TestTargetOs::Windows => "Start-Sleep -Milliseconds 800",
        TestTargetOs::Linux | TestTargetOs::MacOs => "sleep 0.8",
    };
    let default_arguments = serde_json::to_string(&json!({ "command": command }))?;
    let max_arguments = serde_json::to_string(&json!({
        "command": command,
        "timeout_ms": 1_000,
    }))?;
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(DEFAULT_CALL_ID, "shell_command", &default_arguments),
                ev_function_call(MAX_CALL_ID, "shell_command", &max_arguments),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex().with_model("gpt-5.4").with_config(|config| {
        config.tool_execution = ToolExecutionPolicy::new(
            timing_range(
                /*min_ms*/ 100, /*default_ms*/ 200, /*max_ms*/ 500,
            ),
            config.tool_execution.yield_time(),
        );
    });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("run both commands").await?;

    let request = response_mock
        .last_request()
        .context("model should receive both shell results")?;
    let default_output = function_output(&request, DEFAULT_CALL_ID)?;
    let default_elapsed_ms = reported_timeout_ms(&default_output)?;
    assert!(
        (200..=60_000).contains(&default_elapsed_ms),
        "configured 200 ms timeout was reported as {default_elapsed_ms} ms: {default_output}"
    );
    assert!(
        !default_output.contains("Timing policy adjusted"),
        "{default_output}"
    );
    let max_output = function_output(&request, MAX_CALL_ID)?;
    let max_elapsed_ms = reported_timeout_ms(&max_output)?;
    assert!(
        (500..=60_000).contains(&max_elapsed_ms),
        "configured 500 ms timeout was reported as {max_elapsed_ms} ms: {max_output}"
    );
    assert!(
        max_output
            .contains("Timing policy adjusted timeout_ms from 1000 ms to 500 ms (maximum 500 ms)."),
        "{max_output}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_write_stdin_discloses_yield_adjustment() -> Result<()> {
    const CALL_ID: &str = "tool-execution-failed-write-stdin";

    skip_if_no_network!(Ok(()));

    let arguments = serde_json::to_string(&json!({
        "session_id": 999_999,
        "chars": "",
        "yield_time_ms": 1,
    }))?;
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(CALL_ID, "write_stdin", &arguments),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex().with_model("gpt-5.4").with_config(|config| {
        config.tool_execution = ToolExecutionPolicy::new(
            config.tool_execution.timeout(),
            timing_range(
                /*min_ms*/ 200, /*default_ms*/ 300, /*max_ms*/ 500,
            ),
        );
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("unified exec should be enableable");
    });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("write to the missing process").await?;

    let request = response_mock
        .last_request()
        .context("model should receive the write_stdin error")?;
    let output = function_output(&request, CALL_ID)?;
    assert!(
        output
            .contains("Timing policy adjusted yield_time_ms from 1 ms to 200 ms (minimum 200 ms)."),
        "{output}"
    );
    assert!(
        output.contains("write_stdin failed: Unknown process id 999999"),
        "{output}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_code_mode_shell_call_uses_timeout_policy() -> Result<()> {
    const CALL_ID: &str = "tool-execution-nested-shell";

    skip_if_no_network!(Ok(()));

    let (command, minimum_ms) = match test_target_os() {
        TestTargetOs::Windows => (
            "Start-Sleep -Milliseconds 50; Write-Output nested-shell-complete",
            5_000,
        ),
        TestTargetOs::Linux | TestTargetOs::MacOs => {
            ("sleep 0.05; printf nested-shell-complete", 200)
        }
    };
    let command = serde_json::to_string(command)?;
    let code = format!(
        r#"const result = await tools.shell_command({{ command: {command}, timeout_ms: 1 }});
text(result);
"#
    );
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_custom_tool_call(CALL_ID, "exec", &code),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.shell_type = ConfigShellToolType::ShellCommand;
        })
        .with_config(move |config| {
            config.tool_execution = ToolExecutionPolicy::new(
                timing_range(
                    /*min_ms*/ minimum_ms,
                    /*default_ms*/ minimum_ms + 100,
                    /*max_ms*/ minimum_ms + 300,
                ),
                config.tool_execution.yield_time(),
            );
            config
                .features
                .disable(Feature::UnifiedExec)
                .expect("unified exec should be disableable");
            config
                .features
                .enable(Feature::CodeMode)
                .expect("code mode should be enableable");
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("run the nested shell call").await?;

    let request = response_mock
        .last_request()
        .context("model should receive the nested shell result")?;
    let output = request.custom_tool_call_output(CALL_ID).to_string();
    assert!(output.contains("nested-shell-complete"), "{output}");
    assert!(!output.contains("Script running with cell ID"), "{output}");
    assert!(
        output.contains(&format!(
            "Timing policy adjusted timeout_ms from 1 ms to {minimum_ms} ms (minimum {minimum_ms} ms)."
        )),
        "{output}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polls_snapshot_reloaded_policy_and_input_uses_interactive_range() -> Result<()> {
    const EXEC_CALL_ID: &str = "tool-execution-persistent-exec";
    const POLL_CALL_ID: &str = "tool-execution-empty-poll";
    const RELOADED_POLL_CALL_ID: &str = "tool-execution-reloaded-poll";
    const INPUT_CALL_ID: &str = "tool-execution-input";

    skip_if_target_windows!(Ok(()), "uses POSIX PTY behavior");
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));

    let exec_arguments = serde_json::to_string(&json!({
        "cmd": "/bin/cat",
        "tty": true,
        "yield_time_ms": 250,
    }))?;
    let poll_arguments = serde_json::to_string(&json!({
        "session_id": 1_000,
        "chars": "",
        "yield_time_ms": 1_000,
    }))?;
    let input_arguments = serde_json::to_string(&json!({
        "session_id": 1_000,
        "chars": "hello\n",
        "yield_time_ms": 250,
    }))?;
    let reloaded_poll_arguments = serde_json::to_string(&json!({
        "session_id": 1_000,
        "chars": "",
        "yield_time_ms": 1,
    }))?;
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(EXEC_CALL_ID, "exec_command", &exec_arguments),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "started"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_function_call(POLL_CALL_ID, "write_stdin", &poll_arguments),
                ev_completed("resp-3"),
            ]),
            sse(vec![
                ev_response_created("resp-4"),
                ev_assistant_message("msg-2", "still running"),
                ev_completed("resp-4"),
            ]),
            sse(vec![
                ev_response_created("resp-5"),
                ev_function_call(
                    RELOADED_POLL_CALL_ID,
                    "write_stdin",
                    &reloaded_poll_arguments,
                ),
                ev_completed("resp-5"),
            ]),
            sse(vec![
                ev_response_created("resp-6"),
                ev_assistant_message("msg-3", "still running with refreshed policy"),
                ev_completed("resp-6"),
            ]),
            sse(vec![
                ev_response_created("resp-7"),
                ev_function_call(INPUT_CALL_ID, "write_stdin", &input_arguments),
                ev_completed("resp-7"),
            ]),
            sse(vec![
                ev_response_created("resp-8"),
                ev_assistant_message("msg-4", "done"),
                ev_completed("resp-8"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex().with_model("gpt-5.4").with_config(|config| {
        config.tool_execution = ToolExecutionPolicy::new(
            timing_range(
                /*min_ms*/ 50, /*default_ms*/ 75, /*max_ms*/ 100,
            ),
            timing_range(
                /*min_ms*/ 400, /*default_ms*/ 500, /*max_ms*/ 600,
            ),
        );
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("unified exec should be enableable");
    });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("start the persistent command").await?;
    let request = response_mock
        .last_request()
        .context("model should receive the initial exec result")?;
    let output = function_output(&request, EXEC_CALL_ID)?;
    assert!(
        output.contains("Process running with session ID 1000"),
        "{output}"
    );
    assert!(!output.contains("Timing policy adjusted"), "{output}");

    let poll_turn = test.submit_turn("poll without input");
    tokio::pin!(poll_turn);
    tokio::select! {
        result = &mut poll_turn => {
            result?;
            anyhow::bail!("empty poll completed before the policy could be refreshed");
        }
        () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
    }
    let mut refreshed_config = test.config.clone();
    refreshed_config.tool_execution = ToolExecutionPolicy::new(
        refreshed_config.tool_execution.timeout(),
        timing_range(
            /*min_ms*/ 50, /*default_ms*/ 75, /*max_ms*/ 100,
        ),
    );
    test.codex.refresh_runtime_config(refreshed_config).await;
    poll_turn.await?;

    let request = response_mock
        .last_request()
        .context("model should receive the empty poll result")?;
    let output = function_output(&request, POLL_CALL_ID)?;
    assert!(
        output.contains("Process running with session ID 1000"),
        "{output}"
    );
    assert!(
        output.contains(
            "Timing policy adjusted yield_time_ms from 1000 ms to 600 ms (maximum 600 ms)."
        ),
        "{output}"
    );

    test.submit_turn("poll again with refreshed policy").await?;
    let request = response_mock
        .last_request()
        .context("model should receive the reloaded poll result")?;
    let output = function_output(&request, RELOADED_POLL_CALL_ID)?;
    assert!(
        output.contains("Process running with session ID 1000"),
        "{output}"
    );
    assert!(
        output.contains("Timing policy adjusted yield_time_ms from 1 ms to 50 ms (minimum 50 ms)."),
        "{output}"
    );

    test.submit_turn("send input").await?;
    let request = response_mock
        .last_request()
        .context("model should receive the input result")?;
    let output = function_output(&request, INPUT_CALL_ID)?;
    assert!(output.contains("hello"), "{output}");
    assert!(!output.contains("Timing policy adjusted"), "{output}");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refreshed_policy_updates_future_contracts_and_calls() -> Result<()> {
    const CALL_ID: &str = "tool-execution-refreshed-shell";

    skip_if_no_network!(Ok(()));

    let (command, refreshed_minimum_ms) = match test_target_os() {
        TestTargetOs::Windows => ("Write-Output refreshed-policy", 5_000),
        TestTargetOs::Linux | TestTargetOs::MacOs => ("printf refreshed-policy", 500),
    };
    let arguments = serde_json::to_string(&json!({
        "command": command,
        "timeout_ms": 1,
    }))?;
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_assistant_message("msg-1", "ready"),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call(CALL_ID, "shell_command", &arguments),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-2", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.shell_type = ConfigShellToolType::ShellCommand;
        })
        .with_config(|config| {
            config.tool_execution = ToolExecutionPolicy::new(
                timing_range(
                    /*min_ms*/ 200, /*default_ms*/ 300, /*max_ms*/ 400,
                ),
                config.tool_execution.yield_time(),
            );
            config
                .features
                .disable(Feature::UnifiedExec)
                .expect("unified exec should be disableable");
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("show the current tools").await?;

    let mut refreshed_config = test.config.clone();
    refreshed_config.tool_execution = ToolExecutionPolicy::new(
        timing_range(
            /*min_ms*/ refreshed_minimum_ms,
            /*default_ms*/ refreshed_minimum_ms + 100,
            /*max_ms*/ refreshed_minimum_ms + 200,
        ),
        refreshed_config.tool_execution.yield_time(),
    );
    test.codex.refresh_runtime_config(refreshed_config).await;
    test.submit_turn("run the command with refreshed config")
        .await?;

    let requests = response_mock.requests();
    let initial_description =
        tool_property_description(&requests[0], "shell_command", "timeout_ms")?;
    assert!(
        initial_description.contains("default is 300 ms; effective range is 200-400 ms"),
        "{initial_description}"
    );
    let refreshed_description =
        tool_property_description(&requests[1], "shell_command", "timeout_ms")?;
    assert!(
        refreshed_description.contains(&format!(
            "default is {} ms; effective range is {refreshed_minimum_ms}-{} ms",
            refreshed_minimum_ms + 100,
            refreshed_minimum_ms + 200
        )),
        "{refreshed_description}"
    );
    let output = function_output(&requests[2], CALL_ID)?;
    assert!(output.contains("refreshed-policy"), "{output}");
    assert!(
        output.contains(&format!(
            "Timing policy adjusted timeout_ms from 1 ms to {refreshed_minimum_ms} ms (minimum {refreshed_minimum_ms} ms)."
        )),
        "{output}"
    );

    Ok(())
}
