use super::*;
use codex_api::TextControls;
use codex_protocol::ResponseItemId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::MessagePhase;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use std::collections::HashMap;

fn request(
    input: Vec<ResponseItem>,
    instructions: &str,
    text: Option<TextControls>,
) -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "gpt-5".to_string(),
        instructions: instructions.to_string(),
        input,
        tools: None,
        tool_choice: "auto".to_string(),
        parallel_tool_calls: false,
        reasoning: None,
        store: false,
        stream: true,
        stream_options: None,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text,
        client_metadata: None,
    }
}

fn message(text: String) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function(description: String) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: "lookup".to_string(),
        description,
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::default(),
        output_schema: None,
    })
}

#[test]
fn splits_non_replayed_messages_without_losing_text() {
    let text = "x".repeat(max_model_request_item_bytes());
    let id = ResponseItemId::with_suffix("msg", "oversized");
    let metadata = InternalChatMessageMetadataPassthrough {
        turn_id: Some("turn-message".to_string()),
        ..Default::default()
    };
    let mut request = request(
        vec![ResponseItem::Message {
            id: Some(id.clone()),
            role: "user".to_string(),
            content: vec![ContentItem::InputText { text: text.clone() }],
            phase: Some(MessagePhase::Commentary),
            internal_chat_message_metadata_passthrough: Some(metadata.clone()),
        }],
        "",
        /*text*/ None,
    );

    split_model_request_messages(&mut request, &[]).expect("message should be splittable");

    assert!(request.input.len() > 1);
    assert!(
        request
            .input
            .iter()
            .all(|item| estimate_item_token_count(item) <= MAX_MODEL_REQUEST_ITEM_TOKENS as i64)
    );
    assert_eq!(message_input_text(&request.input), text);
    let provenance = request
        .input
        .iter()
        .map(|item| match item {
            ResponseItem::Message {
                id,
                role,
                phase,
                internal_chat_message_metadata_passthrough: metadata,
                ..
            } => (id.clone(), role.clone(), phase.clone(), metadata.clone()),
            other => panic!("expected split message, got {other:?}"),
        })
        .collect::<Vec<_>>();
    let expected = (0..request.input.len())
        .map(|index| {
            (
                (index == 0).then(|| id.clone()),
                "user".to_string(),
                Some(MessagePhase::Commentary),
                Some(metadata.clone()),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(provenance, expected);
}

#[test]
fn trims_only_replayed_prefix_for_exact_cardinality() {
    let prefix = message("prefix".to_string());
    let live = message("live".to_string());
    let mut input = vec![prefix; MAX_MODEL_REQUEST_ITEMS];
    input.push(live.clone());
    let mut request = request(input, "instructions", /*text*/ None);

    bound_replayed_model_request(
        &mut request,
        &[function("tool".to_string())],
        0..MAX_MODEL_REQUEST_ITEMS,
        /*validate_replayed_tools*/ false,
    )
    .expect("replay prefix should make room for live request components");

    assert_eq!(request.input.last(), Some(&live));
    assert!(
        request_item_count(&request, &[function("tool".to_string())]) <= MAX_MODEL_REQUEST_ITEMS
    );
}

#[test]
fn trims_replayed_prefix_for_exact_model_visible_bytes() {
    let replay_item = message("x".repeat(35_000));
    let mut request = request(vec![replay_item; 500], "instructions", /*text*/ None);

    bound_replayed_model_request(
        &mut request,
        &[],
        0..500,
        /*validate_replayed_tools*/ false,
    )
    .expect("replay prefix should be trimmed to the total byte budget");

    assert!(model_visible_request_bytes(&request, &[], &[]).unwrap() <= MAX_MODEL_REQUEST_BYTES);
    assert!(request.input.len() < 500);
}

#[test]
fn splits_live_suffix_messages_in_a_resumed_request() {
    let live_text = "x".repeat(max_model_request_item_bytes());
    let mut request = request(
        vec![message("prefix".to_string()), message(live_text.clone())],
        "",
        /*text*/ None,
    );

    bound_replayed_model_request(
        &mut request,
        &[],
        0..1,
        /*validate_replayed_tools*/ false,
    )
    .expect("live suffix message should be bounded");

    assert_eq!(message_input_text(&request.input[1..]), live_text);
    assert!(
        request.input[1..]
            .iter()
            .all(|item| estimate_item_token_count(item) <= MAX_MODEL_REQUEST_ITEM_TOKENS as i64)
    );
}

#[test]
fn splits_non_replayed_agent_messages_without_losing_text() {
    let text = "x".repeat(max_model_request_item_bytes());
    let id = ResponseItemId::with_suffix("agent", "oversized");
    let metadata = InternalChatMessageMetadataPassthrough {
        turn_id: Some("turn-agent".to_string()),
        ..Default::default()
    };
    let mut request = request(
        vec![ResponseItem::AgentMessage {
            id: Some(id.clone()),
            author: "parent".to_string(),
            recipient: "child".to_string(),
            content: vec![AgentMessageInputContent::InputText { text: text.clone() }],
            internal_chat_message_metadata_passthrough: Some(metadata.clone()),
        }],
        "",
        /*text*/ None,
    );

    split_model_request_messages(&mut request, &[]).expect("agent message should be splittable");

    assert!(request.input.len() > 1);
    assert!(
        request
            .input
            .iter()
            .all(|item| estimate_item_token_count(item) <= MAX_MODEL_REQUEST_ITEM_TOKENS as i64)
    );
    let actual = request
        .input
        .iter()
        .filter_map(|item| match item {
            ResponseItem::AgentMessage { content, .. } => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|content| match content {
            AgentMessageInputContent::InputText { text } => Some(text.as_str()),
            AgentMessageInputContent::EncryptedContent { .. } => None,
        })
        .collect::<String>();
    assert_eq!(actual, text);
    let provenance = request
        .input
        .iter()
        .map(|item| match item {
            ResponseItem::AgentMessage {
                id,
                author,
                recipient,
                internal_chat_message_metadata_passthrough: metadata,
                ..
            } => (
                id.clone(),
                author.clone(),
                recipient.clone(),
                metadata.clone(),
            ),
            other => panic!("expected split agent message, got {other:?}"),
        })
        .collect::<Vec<_>>();
    let expected = (0..request.input.len())
        .map(|index| {
            (
                (index == 0).then(|| id.clone()),
                "parent".to_string(),
                "child".to_string(),
                Some(metadata.clone()),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(provenance, expected);
}

#[test]
fn truncates_tool_outputs_to_the_hard_item_cap_preserving_envelopes() {
    let function_id = ResponseItemId::with_suffix("fco", "oversized");
    let custom_id = ResponseItemId::with_suffix("ctco", "oversized");
    let metadata = InternalChatMessageMetadataPassthrough {
        turn_id: Some("turn-output".to_string()),
        ..Default::default()
    };
    let oversized_payload = FunctionCallOutputPayload {
        body: FunctionCallOutputBody::Text("tool output".repeat(50_000)),
        success: Some(true),
    };
    let sources = vec![
        ResponseItem::FunctionCallOutput {
            id: Some(function_id.clone()),
            call_id: "function-call".to_string(),
            output: oversized_payload.clone(),
            internal_chat_message_metadata_passthrough: Some(metadata.clone()),
        },
        ResponseItem::CustomToolCallOutput {
            id: Some(custom_id.clone()),
            call_id: "custom-call".to_string(),
            name: Some("custom-tool".to_string()),
            output: oversized_payload,
            internal_chat_message_metadata_passthrough: Some(metadata.clone()),
        },
    ];
    let mut request = request(sources.clone(), "", /*text*/ None);

    split_model_request_messages(&mut request, &[]).expect("tool outputs should be truncated");

    assert_eq!(request.input.len(), sources.len());
    assert!(
        request.input.iter().all(|item| {
            estimate_item_token_count(item) <= MAX_MODEL_REQUEST_ITEM_TOKENS as i64
        })
    );
    let output_provenance = request
        .input
        .iter()
        .map(|item| match item {
            ResponseItem::FunctionCallOutput {
                id,
                call_id,
                output,
                internal_chat_message_metadata_passthrough: metadata,
            } => (
                id.clone(),
                call_id.clone(),
                None,
                output.success,
                metadata.clone(),
                output.text_content().map(str::to_string),
            ),
            ResponseItem::CustomToolCallOutput {
                id,
                call_id,
                name,
                output,
                internal_chat_message_metadata_passthrough: metadata,
            } => (
                id.clone(),
                call_id.clone(),
                name.clone(),
                output.success,
                metadata.clone(),
                output.text_content().map(str::to_string),
            ),
            other => panic!("expected tool output, got {other:?}"),
        })
        .collect::<Vec<_>>();
    let expected_envelopes = vec![
        (
            Some(function_id),
            "function-call".to_string(),
            None,
            Some(true),
            Some(metadata.clone()),
        ),
        (
            Some(custom_id),
            "custom-call".to_string(),
            Some("custom-tool".to_string()),
            Some(true),
            Some(metadata),
        ),
    ];
    assert_eq!(
        output_provenance
            .iter()
            .map(|(id, call_id, name, success, metadata, _)| (
                id.clone(),
                call_id.clone(),
                name.clone(),
                *success,
                metadata.clone(),
            ))
            .collect::<Vec<_>>(),
        expected_envelopes
    );
    assert!(output_provenance.iter().all(|(_, _, _, _, _, output)| {
        output
            .as_deref()
            .is_some_and(|output| output.contains("tokens truncated"))
    }));
}

#[test]
fn rejects_tool_outputs_with_unsplittable_fixed_overhead() {
    let oversized_call_id = "call".repeat(max_model_request_item_bytes());
    let mut request = request(
        vec![ResponseItem::FunctionCallOutput {
            id: None,
            call_id: oversized_call_id,
            output: FunctionCallOutputPayload::from_text("small output".to_string()),
            internal_chat_message_metadata_passthrough: None,
        }],
        "",
        /*text*/ None,
    );

    let error = split_model_request_messages(&mut request, &[])
        .expect_err("fixed output envelope should remain subject to the hard item cap");

    assert!(error.to_string().contains("individual input item"));
}

#[test]
fn rejects_tool_outputs_with_unsplittable_structured_content() {
    let sources = vec![
        ResponseItem::FunctionCallOutput {
            id: Some(ResponseItemId::with_suffix("fco", "encrypted")),
            call_id: "encrypted-call".to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::EncryptedContent {
                    encrypted_content: "A".repeat(100_000),
                },
            ]),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput {
            id: Some(ResponseItemId::with_suffix("ctco", "images")),
            call_id: "image-call".to_string(),
            name: Some("image-tool".to_string()),
            output: FunctionCallOutputPayload::from_content_items(
                (0..6)
                    .map(|index| FunctionCallOutputContentItem::InputImage {
                        image_url: format!("data:image/png;base64,image-{index}"),
                        detail: None,
                    })
                    .collect(),
            ),
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    for source in sources {
        let mut request = request(vec![source.clone()], "", /*text*/ None);
        assert!(estimate_item_token_count(&source) > MAX_MODEL_REQUEST_ITEM_TOKENS as i64);

        let error = split_model_request_messages(&mut request, &[])
            .expect_err("fixed structured content should remain subject to the hard item cap");

        assert!(error.to_string().contains("individual input item"));
    }
}

#[test]
fn preserves_bounded_structured_tool_outputs_exactly() {
    let sources = vec![
        ResponseItem::FunctionCallOutput {
            id: Some(ResponseItemId::with_suffix("fco", "encrypted-bounded")),
            call_id: "encrypted-bounded-call".to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::EncryptedContent {
                    encrypted_content: "A".repeat(60_000),
                },
            ]),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput {
            id: Some(ResponseItemId::with_suffix("ctco", "images-bounded")),
            call_id: "image-bounded-call".to_string(),
            name: Some("image-tool".to_string()),
            output: FunctionCallOutputPayload::from_content_items(
                (0..5)
                    .map(|index| FunctionCallOutputContentItem::InputImage {
                        image_url: format!("data:image/png;base64,image-{index}"),
                        detail: None,
                    })
                    .collect(),
            ),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    assert!(sources.iter().all(|source| {
        estimate_item_token_count(source) <= MAX_MODEL_REQUEST_ITEM_TOKENS as i64
    }));
    let mut request = request(sources.clone(), "", /*text*/ None);

    split_model_request_messages(&mut request, &[]).expect("bounded outputs should be preserved");

    assert_eq!(request.input, sources);
}

#[test]
fn rejects_non_replayed_requests_over_the_total_byte_budget() {
    let mut request = request(
        vec![message("x".repeat(35_000)); 500],
        "",
        /*text*/ None,
    );

    let error = split_model_request_messages(&mut request, &[])
        .expect_err("non-replayed requests must respect the total byte budget");

    assert!(error.to_string().contains("bounded context budget"));
}

#[test]
fn rejects_messages_with_unsplittable_fixed_overhead() {
    let oversized = ResponseItem::Message {
        id: None,
        role: "r".repeat(max_model_request_item_bytes()),
        content: vec![ContentItem::InputText {
            text: "text".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let mut request = request(vec![oversized], "", /*text*/ None);

    let error = split_model_request_messages(&mut request, &[])
        .expect_err("fixed message overhead cannot be split safely");

    assert!(error.to_string().contains("individual input item"));
}

#[test]
fn rejects_oversized_unsplittable_media() {
    let oversized = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputAudio {
            audio_url: "a".repeat(max_model_request_item_bytes()),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(estimate_item_token_count(&oversized) > MAX_MODEL_REQUEST_ITEM_TOKENS as i64);
    let mut request = request(vec![oversized], "", /*text*/ None);

    let error = split_model_request_messages(&mut request, &[])
        .expect_err("oversized media cannot be split safely");

    assert!(error.to_string().contains("individual input item"));
}

#[test]
fn replay_byte_bounding_evicts_an_entire_split_message_group() {
    let marker = "oldest-split-message:";
    let oldest = message(format!("{marker}{}", "o".repeat(80_000)));
    let filler = message("f".repeat(35_000));
    let mut input = vec![oldest];
    input.extend(std::iter::repeat_n(filler, 480));
    let replay_items = input.len();
    let mut request = request(input, "", /*text*/ None);

    bound_replayed_model_request(
        &mut request,
        &[],
        0..replay_items,
        /*validate_replayed_tools*/ false,
    )
    .expect("whole replay groups should be evicted to fit the byte limit");

    assert!(!message_input_text(&request.input).contains(marker));
    assert!(model_visible_request_bytes(&request, &[], &[]).unwrap() <= MAX_MODEL_REQUEST_BYTES);
}

fn message_input_text(items: &[ResponseItem]) -> String {
    items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, .. } => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|content| match content {
            ContentItem::InputText { text } => Some(text.as_str()),
            ContentItem::OutputText { .. }
            | ContentItem::InputImage { .. }
            | ContentItem::InputAudio { .. } => None,
        })
        .collect()
}

#[test]
fn validates_an_empty_replay_prefix() {
    let mut request = request(
        vec![message(String::new()); MAX_MODEL_REQUEST_ITEMS + 1],
        "",
        /*text*/ None,
    );

    let error = bound_replayed_model_request(
        &mut request,
        &[],
        0..0,
        /*validate_replayed_tools*/ false,
    )
    .expect_err("empty replay still marks a resumed request for total-budget validation");

    assert!(error.to_string().contains("bounded context budget"));
}

#[test]
fn validates_tools_only_when_they_include_persisted_dynamic_tools() {
    let large_tool = function("x".repeat(max_model_request_item_bytes()));
    let mut request = request(vec![message("replay".to_string())], "", /*text*/ None);

    bound_replayed_model_request(
        &mut request,
        std::slice::from_ref(&large_tool),
        0..1,
        /*validate_replayed_tools*/ false,
    )
    .expect("live tools keep upstream behavior");
    let replay_end = request.input.len();
    let error = bound_replayed_model_request(
        &mut request,
        &[large_tool],
        0..replay_end,
        /*validate_replayed_tools*/ true,
    )
    .expect_err("persisted dynamic tools must fit their final envelope");

    assert!(error.to_string().contains("individual tool definition"));
}

#[test]
fn leaves_live_output_schema_unchanged_in_a_resumed_request() {
    let text = codex_api::create_text_param_for_request(
        /*verbosity*/ None,
        &Some(serde_json::json!({
            "type": "object",
            "description": "x".repeat(max_model_request_item_bytes()),
        })),
        /*output_schema_strict*/ true,
    );
    let mut request = request(vec![message("replay".to_string())], "", text.clone());

    bound_replayed_model_request(
        &mut request,
        &[],
        0..1,
        /*validate_replayed_tools*/ false,
    )
    .expect("live output schemas keep upstream behavior");

    assert_eq!(request.text, text);
}

#[test]
fn ignores_non_model_visible_client_metadata() {
    let replay = message("replay".to_string());
    let mut request = request(vec![replay.clone()], "", /*text*/ None);
    request.client_metadata = Some(HashMap::from([(
        "metadata".to_string(),
        "x".repeat(MAX_MODEL_REQUEST_BYTES),
    )]));

    bound_replayed_model_request(
        &mut request,
        &[],
        0..1,
        /*validate_replayed_tools*/ false,
    )
    .expect("client metadata is not model-visible context");

    assert_eq!(request.input, vec![replay]);
}

#[test]
fn counts_only_retained_input_separators() {
    let first = message("first".to_string());
    let second = message("second".to_string());
    let request = request(vec![first.clone(), second.clone()], "", /*text*/ None);
    let expected = b"{\"input\":[]}".len()
        + model_visible_item_bytes(&first)
        + model_visible_item_bytes(&second)
        + 1;

    assert_eq!(
        model_visible_request_bytes(&request, &[], &[]).unwrap(),
        expected
    );
}
