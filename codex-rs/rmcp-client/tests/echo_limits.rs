use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use codex_rmcp_client::LocalStdioServerLauncher;
use codex_rmcp_client::RmcpClient;
use futures::FutureExt as _;
use pretty_assertions::assert_eq;
use rmcp::model::ClientCapabilities;
use rmcp::model::Implementation;
use rmcp::model::InitializeRequestParams;
use rmcp::model::ProtocolVersion;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn echo_rejects_unbounded_repetition_before_allocating() -> anyhow::Result<()> {
    let server = codex_utils_cargo_bin::cargo_bin("test_stdio_server")?;
    let client = RmcpClient::new_stdio_client(
        server.into(),
        Vec::<OsString>::new(),
        /*env*/ None,
        &[],
        /*cwd*/ None,
        Arc::new(LocalStdioServerLauncher::new(std::env::current_dir()?)),
    )
    .await?;
    client
        .initialize(
            InitializeRequestParams::new(
                ClientCapabilities::default(),
                Implementation::new("echo-limit-test", "0.0.0-test"),
            )
            .with_protocol_version(ProtocolVersion::V_2025_06_18),
            Some(Duration::from_secs(5)),
            Box::new(|_, _| {
                async {
                    Ok(ElicitationResponse {
                        action: ElicitationAction::Decline,
                        content: None,
                        meta: None,
                    })
                }
                .boxed()
            }),
        )
        .await?;

    let tools = client
        .list_tools(/*params*/ None, Some(Duration::from_secs(5)))
        .await?;
    let echo = tools
        .tools
        .iter()
        .find(|tool| tool.name == "echo")
        .expect("echo tool present");
    let input_schema = serde_json::to_value(&*echo.input_schema)?;
    assert_eq!(
        input_schema["properties"]["repeat"]["maximum"],
        json!(100_000)
    );

    let repeat_error = client
        .call_tool(
            "echo".to_string(),
            Some(json!({ "message": "x", "repeat": 100_001 })),
            /*meta*/ None,
            Some(Duration::from_secs(5)),
        )
        .await
        .expect_err("repeat above the advertised maximum must be rejected");
    assert!(
        repeat_error
            .to_string()
            .contains("repeat must not exceed 100000")
    );

    let output_error = client
        .call_tool(
            "echo".to_string(),
            Some(json!({ "message": "0123456789a", "repeat": 100_000 })),
            /*meta*/ None,
            Some(Duration::from_secs(5)),
        )
        .await
        .expect_err("echo output above the byte cap must be rejected");
    assert!(
        output_error
            .to_string()
            .contains("echo output must not exceed 1048576 bytes")
    );

    let result = client
        .call_tool(
            "echo".to_string(),
            Some(json!({ "message": "x", "repeat": 100_000 })),
            /*meta*/ None,
            Some(Duration::from_secs(5)),
        )
        .await?;
    assert_eq!(
        result.structured_content,
        Some(json!({
            "echo": format!("ECHOING: {}", "x".repeat(100_000)),
            "env": serde_json::Value::Null,
        }))
    );

    client.shutdown().await;
    Ok(())
}
