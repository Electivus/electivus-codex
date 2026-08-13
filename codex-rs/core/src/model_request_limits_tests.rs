use super::*;
use codex_api::TextControls;
use codex_protocol::models::ContentItem;
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
fn leaves_non_replayed_requests_unchanged() {
    let oversized = message("x".repeat(max_model_request_item_bytes()));
    let request = request(vec![oversized.clone()], "", /*text*/ None);

    assert_eq!(request.input, vec![oversized]);
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
fn leaves_live_suffix_items_unchanged_in_a_resumed_request() {
    let live = message("x".repeat(max_model_request_item_bytes()));
    let mut request = request(
        vec![message("prefix".to_string()), live.clone()],
        "",
        /*text*/ None,
    );

    bound_replayed_model_request(
        &mut request,
        &[],
        0..1,
        /*validate_replayed_tools*/ false,
    )
    .expect("live suffix keeps upstream behavior");

    assert_eq!(request.input.last(), Some(&live));
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
