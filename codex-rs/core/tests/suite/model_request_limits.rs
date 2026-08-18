use anyhow::Result;
use codex_core::StartThreadOptions;
use codex_history::InitialHistory;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutItem;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;

fn user_message(text: String) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText { text }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_request_splits_messages_and_trims_only_replay_with_responses_lite() -> Result<()> {
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
        model.context_window = Some(10_000_000);
        model.auto_compact_token_limit = Some(9_000_000);
    });
    let test = builder.build_with_auto_env(&server).await?;
    let oversized_text = format!("oversized-replay:{}", "x".repeat(45_000));
    let mut history = vec![user_message("oldest-replay-marker".to_string())];
    history.extend(std::iter::repeat_n(user_message("f".repeat(35_000)), 500));
    history.push(user_message(oversized_text.clone()));
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            initial_history: InitialHistory::Forked(history),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?
        .thread;

    thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "new-live-message".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let request = response_mock.single_request();
    let input = request.input();
    assert!(input.len() <= 10_000);
    assert!(request.body_contains_text("new-live-message"));
    let user_texts = request.message_input_texts("user");
    assert!(!user_texts.iter().any(|text| text == "oldest-replay-marker"));
    assert_eq!(
        input.first().and_then(|item| item["type"].as_str()),
        Some("additional_tools")
    );
    assert_eq!(
        input.get(1).and_then(|item| item["role"].as_str()),
        Some("developer")
    );
    assert!(
        input
            .iter()
            .all(|item| serde_json::to_vec(item).is_ok_and(|item| item.len() <= 40_000))
    );
    let replayed_text = user_texts.concat();
    assert!(replayed_text.contains(&oversized_text));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_request_keeps_oversized_live_dynamic_tool_unchanged() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let marker = "live-dynamic-tool-marker";
    let test = test_codex().build_with_auto_env(&server).await?;
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            initial_history: InitialHistory::Forked(vec![user_message(
                "replayed-message".to_string(),
            )]),
            dynamic_tools: vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
                name: "live_tool".to_string(),
                description: format!("{marker}{}", "x".repeat(45_000)),
                input_schema: serde_json::json!({"type": "object"}),
                defer_loading: false,
            })],
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?
        .thread;

    thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "use the live tool".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    assert!(response_mock.single_request().body_contains_text(marker));
    Ok(())
}
