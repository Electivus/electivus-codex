#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use anyhow::Context;
use anyhow::Result;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use codex_core::TurnInputRequest;
use codex_history::RolloutItem;
use codex_history::RolloutLine;
use codex_model_context::estimate_response_item_model_visible_bytes;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::TempDirExt;
use core_test_support::assert_regex_match;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::stdio_server_bin;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_mcp_server;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

fn assert_wall_time_header(output: &str) {
    let (wall_time, marker) = output
        .split_once('\n')
        .expect("wall-time header should contain an Output marker");
    assert_regex_match(r"^Wall time: [0-9]+(?:\.[0-9]+)? seconds$", wall_time);
    assert_eq!(marker, "Output:");
}

// Verifies that a standard tool call (shell_command) exceeding the model formatting
// limits is truncated before being sent back to the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_call_output_configured_limit_chars_type() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    // Use a model that exposes the shell_command tool.
    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        config.tool_output_token_limit = Some(100_000);
    });

    let fixture = builder.build(&server).await?;

    let call_id = "shell-too-large";
    let command = if cfg!(windows) {
        "for ($i=1; $i -le 100000; $i++) { Write-Output $i }"
    } else {
        "seq 1 100000"
    };
    let args = serde_json::json!({
        "command": command,
        "timeout_ms": 5_000,
    });

    // First response: model tells us to run the tool; second: complete the turn.
    mount_sse_once(
        &server,
        sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock2 = mount_sse_once(
        &server,
        sse(vec![
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile(
            "trigger big shell output",
            PermissionProfile::Disabled,
        )
        .await?;

    // Inspect what we sent back to the model; it should contain a truncated
    // function_call_output for the shell call.
    let request = mock2.single_request();
    let output_item: ResponseItem = serde_json::from_value(
        request
            .inputs_of_type("function_call_output")
            .into_iter()
            .next()
            .context("function_call_output item present for shell call")?,
    )?;
    let output = request
        .function_call_output_text(call_id)
        .context("function_call_output present for shell call")?;
    let output = output.replace("\r\n", "\n");

    // Expect the projected output to remain plain text rather than JSON.
    assert!(
        serde_json::from_str::<Value>(&output).is_err(),
        "expected truncated shell output to be plain text"
    );

    assert!(
        output.contains("tokens truncated"),
        "shell output should contain tokens truncated marker: {output}"
    );
    assert!(estimate_response_item_model_visible_bytes(&output_item) <= 40_000);

    let home = Arc::clone(&fixture.home);
    let rollout_path = fixture
        .session_configured
        .rollout_path
        .clone()
        .context("rollout path")?;
    fixture.codex.flush_rollout().await?;
    let persisted_items = std::fs::read_to_string(&rollout_path)?
        .lines()
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|line| match line.item {
            RolloutItem::ResponseItem(envelope) => Some(envelope.item),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        persisted_items
            .iter()
            .filter(|item| matches!(
                item,
                ResponseItem::FunctionCall { call_id: item_call_id, .. }
                    if item_call_id == call_id
            ))
            .count(),
        1,
        "rollout should preserve one matching function call"
    );
    let persisted_outputs = persisted_items
        .iter()
        .filter(|item| {
            matches!(
                item,
                ResponseItem::FunctionCallOutput { call_id: item_call_id, .. }
                    if item_call_id == call_id
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        persisted_outputs.len(),
        1,
        "rollout should preserve one atomic function output"
    );
    assert!(
        estimate_response_item_model_visible_bytes(persisted_outputs[0]) > 40_000,
        "request-only truncation must not rewrite the persisted output"
    );
    let persisted_output = match persisted_outputs[0] {
        ResponseItem::FunctionCallOutput { output, .. } => output,
        other => unreachable!("filtered function output changed variant: {other:?}"),
    };
    let persisted_text = persisted_output
        .text_content()
        .context("persisted shell output should contain text")?;
    assert!(
        (400_000..=401_000).contains(&persisted_text.len()),
        "configured producer limit should remain durable, got {} bytes",
        persisted_text.len()
    );

    fixture.codex.shutdown_and_wait().await?;
    let resumed_mock = mount_sse_once(
        &server,
        sse(vec![
            responses::ev_assistant_message("msg-resumed", "resumed"),
            responses::ev_completed("resp-resumed"),
        ]),
    )
    .await;
    let mut resume_builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        config.tool_output_token_limit = Some(100_000);
    });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    resumed.submit_turn("continue after cold resume").await?;

    let resumed_request = resumed_mock.single_request();
    assert!(resumed_request.has_function_call(call_id));
    let resumed_outputs = resumed_request
        .inputs_of_type("function_call_output")
        .into_iter()
        .filter(|item| item.get("call_id").and_then(Value::as_str) == Some(call_id))
        .collect::<Vec<_>>();
    assert_eq!(
        resumed_outputs.len(),
        1,
        "cold resume should replay one matching function output"
    );
    let resumed_output: ResponseItem = serde_json::from_value(resumed_outputs[0].clone())?;
    assert!(estimate_response_item_model_visible_bytes(&resumed_output) <= 40_000);
    pretty_assertions::assert_eq!(resumed_output, output_item);
    assert!(
        resumed_request
            .function_call_output_text(call_id)
            .is_some_and(|output| output.contains("tokens truncated"))
    );

    Ok(())
}

// Verifies that a standard tool call (shell_command) exceeding the model formatting
// limits is truncated before being sent back to the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_call_output_exceeds_limit_truncated_chars_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    // Use a model that exposes the shell_command tool.
    let mut builder = test_codex().with_model("gpt-5.2");

    let fixture = builder.build(&server).await?;

    let call_id = "shell-too-large";
    let command = if cfg!(windows) {
        "for ($i=1; $i -le 100000; $i++) { Write-Output $i }"
    } else {
        "seq 1 100000"
    };
    let args = serde_json::json!({
        "command": command,
        "timeout_ms": 5_000,
    });

    // First response: model tells us to run the tool; second: complete the turn.
    mount_sse_once(
        &server,
        sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock2 = mount_sse_once(
        &server,
        sse(vec![
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile(
            "trigger big shell output",
            PermissionProfile::Disabled,
        )
        .await?;

    // Inspect what we sent back to the model; it should contain a truncated
    // function_call_output for the shell call.
    let output = mock2
        .single_request()
        .function_call_output_text(call_id)
        .context("function_call_output present for shell call")?;
    let output = output.replace("\r\n", "\n");

    // Expect plain text (not JSON) containing the entire shell output.
    assert!(
        serde_json::from_str::<Value>(&output).is_err(),
        "expected truncated shell output to be plain text"
    );

    let truncated_pattern = r#"(?s)^Exit code: 0\nWall time: [0-9]+(?:\.[0-9]+)? seconds\nTotal output lines: 100000\nOutput:\n.*?…\d+ chars truncated….*$"#;

    assert_regex_match(truncated_pattern, &output);

    let len = output.len();
    assert!(
        (9_900..=10_100).contains(&len),
        "expected ~10k chars after truncation, got {len}"
    );

    Ok(())
}

// Verifies that a standard tool call (shell_command) exceeding the model formatting
// limits is truncated before being sent back to the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_call_output_exceeds_limit_truncated_for_model() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    // Use a model that exposes the shell_command tool.
    let mut builder = test_codex().with_model("gpt-5.4");
    let fixture = builder.build(&server).await?;

    let call_id = "shell-too-large";
    let command = if cfg!(windows) {
        "for ($i=1; $i -le 100000; $i++) { Write-Output $i }"
    } else {
        "seq 1 100000"
    };
    let args = serde_json::json!({
        "command": command,
        "timeout_ms": 5_000,
    });

    // First response: model tells us to run the tool; second: complete the turn.
    mount_sse_once(
        &server,
        sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock2 = mount_sse_once(
        &server,
        sse(vec![
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile(
            "trigger big shell output",
            PermissionProfile::Disabled,
        )
        .await?;

    // Inspect what we sent back to the model; it should contain a truncated
    // function_call_output for the shell call.
    let output = mock2
        .single_request()
        .function_call_output_text(call_id)
        .context("function_call_output present for shell call")?;
    let output = output.replace("\r\n", "\n");

    // Expect plain text (not JSON) containing the entire shell output.
    assert!(
        serde_json::from_str::<Value>(&output).is_err(),
        "expected truncated shell output to be plain text"
    );
    let truncated_pattern = r#"(?s)^Exit code: 0
Wall time: [0-9]+(?:\.[0-9]+)? seconds
Total output lines: 100000
Output:
1
2
3
4
5
6
.*…[0-9]+ tokens truncated.*
99999
100000
$"#;
    assert_regex_match(truncated_pattern, &output);

    Ok(())
}

// Ensures shell_command outputs that exceed the line limit are truncated only once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_call_output_truncated_only_once() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let mut builder = test_codex().with_model("gpt-5.4");
    let fixture = builder.build(&server).await?;
    let call_id = "shell-single-truncation";
    let command = if cfg!(windows) {
        "for ($i=1; $i -le 10000; $i++) { Write-Output $i }"
    } else {
        "seq 1 10000"
    };
    let args = serde_json::json!({
        "command": command,
        "timeout_ms": 5_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock2 = mount_sse_once(
        &server,
        sse(vec![
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile(
            "trigger big shell output",
            PermissionProfile::Disabled,
        )
        .await?;

    let output = mock2
        .single_request()
        .function_call_output_text(call_id)
        .context("function_call_output present for shell call")?;

    let truncation_markers = output.matches("tokens truncated").count();

    assert_eq!(
        truncation_markers, 1,
        "shell output should carry only one truncation marker: {output}"
    );

    Ok(())
}

// Verifies that an MCP tool call result exceeding the model formatting limits
// is truncated before being sent back to the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn mcp_tool_call_output_exceeds_limit_truncated_for_model() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let call_id = "rmcp-truncated";
    let server_name = "rmcp";
    let namespace = format!("mcp__{server_name}");

    // Build a very large message to exceed 10KiB once serialized.
    let large_msg = "long-message-with-newlines-".repeat(6000);
    let args_json = serde_json::json!({ "message": large_msg });

    mount_sse_once(
        &server,
        sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(
                call_id,
                &namespace,
                "echo",
                &args_json.to_string(),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock2 = mount_sse_once(
        &server,
        sse(vec![
            responses::ev_assistant_message("msg-1", "rmcp echo tool completed."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    // Compile the rmcp stdio test server and configure it.
    let rmcp_test_server_bin = stdio_server_bin()?;

    let mut builder = test_codex().with_config(move |config| {
        let mut servers = config.mcp_servers.get().clone();
        servers.insert(
            server_name.to_string(),
            codex_config::types::McpServerConfig {
                auth: Default::default(),
                transport: codex_config::types::McpServerTransportConfig::Stdio {
                    command: rmcp_test_server_bin,
                    args: Vec::new(),
                    env: None,
                    env_vars: Vec::new(),
                    cwd: None,
                },
                environment_id: "local".to_string(),
                enabled: true,
                required: false,
                supports_parallel_tool_calls: false,
                omit_tools_from: None,
                disabled_reason: None,
                startup_timeout_sec: Some(std::time::Duration::from_secs(10)),
                tool_timeout_sec: None,
                default_tools_approval_mode: None,
                enabled_tools: None,
                disabled_tools: None,
                scopes: None,
                oauth: None,
                oauth_resource: None,
                tools: HashMap::new(),
            },
        );
        config
            .mcp_servers
            .set(servers)
            .expect("test mcp servers should accept any configuration");
        config.tool_output_token_limit = Some(500);
    });
    let fixture = builder.build(&server).await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    fixture
        .submit_turn_with_permission_profile(
            "call the rmcp echo tool with a very large message",
            PermissionProfile::read_only(),
        )
        .await?;

    // The MCP tool call output is converted to a function_call_output for the model.
    let output = mock2
        .single_request()
        .function_call_output_text(call_id)
        .context("function_call_output present for rmcp call")?;

    assert!(
        !output.contains("Total output lines:"),
        "MCP output should not include line-based truncation header: {output}"
    );

    let truncated_pattern = r#"(?s)^Wall time: [0-9]+(?:\.[0-9]+)? seconds\nOutput:\n\{"echo":\s*"ECHOING: long-message-with-newlines-.*tokens truncated.*long-message-with-newlines-.*$"#;
    assert_regex_match(truncated_pattern, &output);
    assert!(output.len() < 2600, "{}", output.len());

    Ok(())
}

// Verifies that an MCP image tool output is serialized as content_items array with
// the image preserved and no truncation summary appended (since there are no text items).
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn mcp_image_output_preserves_image_and_no_text_summary() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let call_id = "rmcp-image-no-trunc";
    let server_name = "rmcp";
    let namespace = format!("mcp__{server_name}");

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(call_id, &namespace, "image", "{}"),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let final_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    // Build the stdio rmcp server and pass a tiny PNG via data URL so it can construct ImageContent.
    let rmcp_test_server_bin = stdio_server_bin()?;

    // 1x1 PNG data URL
    let openai_png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    let mut builder = test_codex().with_config(move |config| {
        let mut servers = config.mcp_servers.get().clone();
        servers.insert(
            server_name.to_string(),
            McpServerConfig {
                auth: Default::default(),
                transport: McpServerTransportConfig::Stdio {
                    command: rmcp_test_server_bin,
                    args: Vec::new(),
                    env: Some(HashMap::from([(
                        "MCP_TEST_IMAGE_DATA_URL".to_string(),
                        openai_png.to_string(),
                    )])),
                    env_vars: Vec::new(),
                    cwd: None,
                },
                environment_id: "local".to_string(),
                enabled: true,
                required: false,
                supports_parallel_tool_calls: false,
                omit_tools_from: None,
                disabled_reason: None,
                startup_timeout_sec: Some(Duration::from_secs(10)),
                tool_timeout_sec: None,
                default_tools_approval_mode: None,
                enabled_tools: None,
                disabled_tools: None,
                scopes: None,
                oauth: None,
                oauth_resource: None,
                tools: HashMap::new(),
            },
        );
        config
            .mcp_servers
            .set(servers)
            .expect("test mcp servers should accept any configuration");
    });
    let fixture = builder.build(&server).await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;
    let session_model = fixture.session_configured.model.clone();
    let permission_profile = PermissionProfile::read_only();
    let sandbox_policy = permission_profile.to_legacy_sandbox_policy(fixture.cwd.path())?;

    fixture
        .codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "call the rmcp image tool".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(local_selections(fixture.cwd.abs())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile: Some(permission_profile),
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: session_model,
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;

    // Wait for completion to ensure the outbound request is captured.
    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    let output_item = final_mock.single_request().function_call_output(call_id);
    // Expect exactly the wall-time text and image item; no trailing truncation summary.
    let output = output_item.get("output").expect("output");
    assert!(output.is_array(), "expected array output");
    let arr = output.as_array().unwrap();
    assert_eq!(arr.len(), 2, "no truncation summary should be appended");
    assert_wall_time_header(
        arr[0]["text"]
            .as_str()
            .expect("first MCP image output item should be wall-time text"),
    );
    assert_eq!(
        arr[1],
        json!({"type": "input_image", "image_url": openai_png, "detail": "high"})
    );

    Ok(())
}

// Token-based policy should report token counts even when truncation is byte-estimated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_policy_marker_reports_tokens() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.4").with_config(|config| {
        config.tool_output_token_limit = Some(50); // small budget to force truncation
    });
    let fixture = builder.build(&server).await?;

    let call_id = "shell-token-marker";
    let args = json!({
        "command": "seq 1 150",
        "timeout_ms": 5_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let done_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile("run the shell tool", PermissionProfile::Disabled)
        .await?;

    let output = done_mock
        .single_request()
        .function_call_output_text(call_id)
        .context("shell output present")?;

    let pattern = r"(?s)^Exit code: 0\nWall time: [0-9]+(?:\.[0-9]+)? seconds\nTotal output lines: 150\nOutput:\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19.*tokens truncated.*129\n130\n131\n132\n133\n134\n135\n136\n137\n138\n139\n140\n141\n142\n143\n144\n145\n146\n147\n148\n149\n150\n$";

    assert_regex_match(pattern, &output);

    Ok(())
}

// Byte-based policy should report bytes removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn byte_policy_marker_reports_bytes() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        config.tool_output_token_limit = Some(50); // ~200 byte cap
    });
    let fixture = builder.build(&server).await?;

    let call_id = "shell-byte-marker";
    let args = json!({
        "command": "seq 1 150",
        "timeout_ms": 5_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let done_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile("run the shell tool", PermissionProfile::Disabled)
        .await?;

    let output = done_mock
        .single_request()
        .function_call_output_text(call_id)
        .context("shell output present")?;

    let pattern = r"(?s)^Exit code: 0\nWall time: [0-9]+(?:\.[0-9]+)? seconds\nTotal output lines: 150\nOutput:\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19.*chars truncated.*129\n130\n131\n132\n133\n134\n135\n136\n137\n138\n139\n140\n141\n142\n143\n144\n145\n146\n147\n148\n149\n150\n$";

    assert_regex_match(pattern, &output);

    Ok(())
}

// shell_command output should remain intact when the config opts into a large token budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_command_output_not_truncated_with_custom_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.4").with_config(|config| {
        config.tool_output_token_limit = Some(50_000); // ample budget
    });
    let fixture = builder.build(&server).await?;

    let call_id = "shell-no-trunc";
    let args = json!({
        "command": "seq 1 1000",
        "timeout_ms": 5_000,
    });
    let expected_body: String = (1..=1000).map(|i| format!("{i}\n")).collect();

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let done_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .submit_turn_with_permission_profile(
            "run big output without truncation",
            PermissionProfile::Disabled,
        )
        .await?;

    let output = done_mock
        .single_request()
        .function_call_output_text(call_id)
        .context("shell output present")?;

    assert!(
        output.ends_with(&expected_body),
        "expected entire shell output when budget increased: {output}"
    );
    assert!(
        !output.contains("truncated"),
        "output should remain untruncated with ample budget"
    );

    Ok(())
}

// The configured MCP output limit may exceed the mandatory per-request item cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn mcp_tool_call_output_obeys_hard_cap_with_custom_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let call_id = "rmcp-untruncated";
    let server_name = "rmcp";
    let namespace = format!("mcp__{server_name}");
    let large_msg = "a".repeat(80_000);
    let args_json = serde_json::json!({ "message": large_msg });

    mount_sse_once(
        &server,
        sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(
                call_id,
                &namespace,
                "echo",
                &args_json.to_string(),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock2 = mount_sse_once(
        &server,
        sse(vec![
            responses::ev_assistant_message("msg-1", "rmcp echo tool completed."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let rmcp_test_server_bin = stdio_server_bin()?;

    let mut builder = test_codex().with_config(move |config| {
        config.tool_output_token_limit = Some(50_000);
        let mut servers = config.mcp_servers.get().clone();
        servers.insert(
            server_name.to_string(),
            codex_config::types::McpServerConfig {
                auth: Default::default(),
                transport: codex_config::types::McpServerTransportConfig::Stdio {
                    command: rmcp_test_server_bin,
                    args: Vec::new(),
                    env: None,
                    env_vars: Vec::new(),
                    cwd: None,
                },
                environment_id: "local".to_string(),
                enabled: true,
                required: false,
                supports_parallel_tool_calls: false,
                omit_tools_from: None,
                disabled_reason: None,
                startup_timeout_sec: Some(std::time::Duration::from_secs(10)),
                tool_timeout_sec: None,
                default_tools_approval_mode: None,
                enabled_tools: None,
                disabled_tools: None,
                scopes: None,
                oauth: None,
                oauth_resource: None,
                tools: HashMap::new(),
            },
        );
        config
            .mcp_servers
            .set(servers)
            .expect("test mcp servers should accept any configuration");
    });
    let fixture = builder.build(&server).await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    fixture
        .submit_turn_with_permission_profile(
            "call the rmcp echo tool with a very large message",
            PermissionProfile::read_only(),
        )
        .await?;

    let request = mock2.single_request();
    let output_item: ResponseItem = serde_json::from_value(
        request
            .inputs_of_type("function_call_output")
            .into_iter()
            .next()
            .context("function_call_output item present for rmcp call")?,
    )?;
    let output = request
        .function_call_output_text(call_id)
        .context("function_call_output present for rmcp call")?;

    assert!(
        output.contains("tokens truncated"),
        "hard-capped MCP output should include the token marker: {output}"
    );
    assert!(estimate_response_item_model_visible_bytes(&output_item) <= 40_000);

    Ok(())
}
