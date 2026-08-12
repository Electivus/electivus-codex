use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_core::StartThreadOptions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::InputModality;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

fn user_message(text: String) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn large_valid_png_data_url() -> Result<String> {
    let mut png = BASE64_STANDARD.decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==",
    )?;
    png.extend(std::iter::repeat_n(0, 80_000));
    Ok(format!(
        "data:image/png;base64,{}",
        BASE64_STANDARD.encode(png)
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_api_request_accepts_multimodal_input_and_bounds_schema_and_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let mut builder = test_codex().with_model_info_override("gpt-5.4", |model| {
        model.input_modalities = vec![InputModality::Text, InputModality::Image];
    });
    let test = builder.build_with_auto_env(&server).await?;
    let image_url = large_valid_png_data_url()?;
    let output_schema = json!({
        "type": "object",
        "properties": { "answer": { "type": "string" } },
        "required": ["answer"],
        "additionalProperties": false
    });

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Image {
                image_url: image_url.clone(),
                detail: None,
            }],
            final_output_json_schema: Some(output_schema.clone()),
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let request = response_mock.single_request();
    let body = request.body_json();
    assert_eq!(request.message_input_image_urls("user"), vec![image_url]);
    assert_eq!(body["text"]["format"]["schema"], output_schema);
    assert!(
        body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_responses_lite_request_bounds_the_additional_tools_item() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let mut builder = test_codex().with_model_info_override("gpt-5.4", |model| {
        model.use_responses_lite = true;
    });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("inspect the final Lite request").await?;
    let body = response_mock.single_request().body_json();
    assert!(body.get("tools").is_none());
    let additional_tools = body["input"]
        .as_array()
        .and_then(|input| input.first())
        .context("Responses Lite request should start with an input item")?;
    assert_eq!(additional_tools["type"], "additional_tools");
    assert!(
        additional_tools["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_request_rejects_more_than_sixteen_mib_before_network() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![responses::ev_completed("unexpected-response")]),
    )
    .await;
    let mut builder = test_codex().with_model_info_override("gpt-5.4", |model| {
        model.context_window = Some(10_000_000);
        model.auto_compact_token_limit = Some(9_000_000);
    });
    let test = builder.build_with_auto_env(&server).await?;
    let history = (0..490)
        .map(|_| RolloutItem::ResponseItem(user_message("x".repeat(35_000))))
        .collect();
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            initial_history: InitialHistory::Forked(history),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?
        .thread;

    thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "trigger the final request".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let EventMsg::Error(error) =
        wait_for_event(&thread, |event| matches!(event, EventMsg::Error(_))).await
    else {
        unreachable!("wait predicate requires an error event")
    };

    assert!(
        error
            .message
            .contains("request exceeds the bounded context budget")
    );
    assert!(response_mock.requests().is_empty());
    Ok(())
}
