use anyhow::Result;
use codex_core::StartThreadOptions;
use codex_features::Feature;
use codex_history::InitialHistory;
use codex_model_context::estimate_response_item_model_visible_bytes;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutRecorder;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;

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
async fn fresh_oversized_user_turn_is_split_only_in_the_model_request() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| config.include_environment_context = false)
        .with_model_info_override("gpt-5.4", |model| {
            model.context_window = Some(1_000_000);
            model.auto_compact_token_limit = Some(900_000);
        })
        .build_with_auto_env(&server)
        .await?;
    let oversized_text = format!("fresh-oversized-user:{}", "x".repeat(90_000));

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: oversized_text.clone(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let request = response_mock.single_request();
    let request_input = request.input();
    let projected_user_items = request_input
        .iter()
        .filter(|item| item["role"].as_str() == Some("user"))
        .collect::<Vec<_>>();
    assert!(projected_user_items.len() > 1);
    assert!(
        projected_user_items
            .iter()
            .all(|item| serde_json::to_vec(item).is_ok_and(|item| item.len() <= 40_000))
    );
    assert_eq!(request.message_input_texts("user").concat(), oversized_text);

    test.codex.flush_rollout().await?;
    let rollout =
        RolloutRecorder::get_rollout_history(&test.codex.rollout_path().expect("rollout path"))
            .await?;
    let persisted_user_texts = rollout
        .get_rollout_items()
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(envelope) => match &envelope.item {
                ResponseItem::Message { role, content, .. } if role == "user" => Some(
                    content
                        .iter()
                        .filter_map(|content| match content {
                            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                                Some(text.clone())
                            }
                            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => None,
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(persisted_user_texts, vec![vec![oversized_text]]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_request_projects_oversized_tool_media_as_one_atomic_output() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_model("gpt-5.4")
        .with_config(|config| config.include_environment_context = false)
        .build_with_auto_env(&server)
        .await?;
    let call_id = "resumed-media-call";
    let image_url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
    let history = vec![
        RolloutItem::ResponseItem(
            ResponseItem::CustomToolCall {
                id: None,
                status: Some("completed".to_string()),
                call_id: call_id.to_string(),
                name: "media_tool".to_string(),
                namespace: None,
                input: "{}".to_string(),
                internal_chat_message_metadata_passthrough: None,
            }
            .into(),
        ),
        RolloutItem::ResponseItem(
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: call_id.to_string(),
                name: Some("media_tool".to_string()),
                output: FunctionCallOutputPayload::from_content_items(
                    (0..6)
                        .map(|_| FunctionCallOutputContentItem::InputImage {
                            image_url: image_url.to_string(),
                            detail: None,
                        })
                        .collect(),
                ),
                internal_chat_message_metadata_passthrough: None,
            }
            .into(),
        ),
    ];
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
            text: "continue after media output".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let request = response_mock.single_request();
    assert_eq!(request.inputs_of_type("custom_tool_call").len(), 1);
    assert_eq!(request.inputs_of_type("custom_tool_call_output").len(), 1);
    let projected: ResponseItem = serde_json::from_value(request.custom_tool_call_output(call_id))?;
    assert!(estimate_response_item_model_visible_bytes(&projected) <= 40_000);
    let ResponseItem::CustomToolCallOutput {
        call_id: projected_call_id,
        output,
        ..
    } = projected
    else {
        unreachable!("custom tool output helper returned another variant");
    };
    assert_eq!(projected_call_id, call_id);
    let content = output.content_items().expect("structured projected output");
    assert!(content.iter().any(|item| matches!(
        item,
        FunctionCallOutputContentItem::InputText { text }
            if text.contains("omitted structured tool output content")
    )));
    assert!(
        (1..6).contains(
            &content
                .iter()
                .filter(|item| matches!(item, FunctionCallOutputContentItem::InputImage { .. }))
                .count()
        )
    );
    Ok(())
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
async fn responses_lite_splits_aggregated_tools_for_turns_and_remote_compaction() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let compact_mock =
        responses::mount_compact_json_once(&server, serde_json::json!({ "output": [] })).await;
    let mut builder = test_codex()
        .with_model_info_override("gpt-5.4", |model| {
            model.use_responses_lite = true;
            model.context_window = Some(1_000_000);
            model.auto_compact_token_limit = Some(900_000);
        })
        .with_config(|config| {
            let _ = config.features.disable(Feature::RemoteCompactionV2);
        });
    let test = builder.build_with_auto_env(&server).await?;
    let dynamic_tools = (0..2)
        .map(|index| {
            DynamicToolSpec::Function(DynamicToolFunctionSpec {
                name: format!("large_dynamic_tool_{index}"),
                description: "x".repeat(17_000),
                input_schema: serde_json::json!({"type": "object"}),
                defer_loading: false,
            })
        })
        .collect();
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            initial_history: InitialHistory::Forked(vec![user_message(
                "replayed-message".to_string(),
            )]),
            dynamic_tools,
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?
        .thread;

    thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "continue with the available tools".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let turn_event = wait_for_event(&thread, |event| {
        matches!(event, EventMsg::TurnComplete(_) | EventMsg::Error(_))
    })
    .await;
    if let EventMsg::Error(error) = turn_event {
        panic!("Responses Lite turn failed before sampling: {error:?}");
    }
    thread.submit(Op::Compact).await?;
    let compact_event = wait_for_event(&thread, |event| {
        matches!(event, EventMsg::TurnComplete(_) | EventMsg::Error(_))
    })
    .await;
    if let EventMsg::Error(error) = compact_event {
        panic!("Responses Lite compaction failed before sampling: {error:?}");
    }

    for request in [
        response_mock.single_request(),
        compact_mock.single_request(),
    ] {
        let input = request.input();
        let additional_tools = input
            .iter()
            .filter(|item| item["type"].as_str() == Some("additional_tools"))
            .collect::<Vec<_>>();
        assert!(additional_tools.len() > 1);
        assert!(
            input
                .iter()
                .all(|item| serde_json::to_vec(item).is_ok_and(|item| item.len() <= 40_000))
        );
        let tool_names = additional_tools
            .iter()
            .flat_map(|item| {
                item["tools"]
                    .as_array()
                    .into_iter()
                    .flat_map(|tools| tools.iter())
            })
            .flat_map(|tool| {
                tool["tools"]
                    .as_array()
                    .map(|tools| tools.iter().collect::<Vec<_>>())
                    .unwrap_or_else(|| vec![tool])
            })
            .filter_map(|tool| tool["name"].as_str())
            .filter(|name| name.starts_with("large_dynamic_tool_"))
            .collect::<Vec<_>>();
        assert_eq!(
            tool_names,
            vec!["large_dynamic_tool_0", "large_dynamic_tool_1"]
        );
        assert!(request.body_json().get("tools").is_none());
        assert_eq!(
            request.body_json()["parallel_tool_calls"],
            Value::Bool(false)
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_request_rejects_oversized_live_dynamic_tool_before_sampling() -> Result<()> {
    let server = responses::start_mock_server().await;
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
    let EventMsg::Error(error) =
        wait_for_event(&thread, |event| matches!(event, EventMsg::Error(_))).await
    else {
        unreachable!("wait predicate only accepts error events");
    };

    assert!(error.message.contains("individual tool definition"));
    Ok(())
}
