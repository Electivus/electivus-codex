use super::*;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use serde_json::json;

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

fn function(name: &str, description: String) -> ResponsesApiTool {
    ResponsesApiTool {
        name: name.to_string(),
        description,
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::default(),
        output_schema: None,
    }
}

#[test]
fn accepts_bounded_final_request_items() {
    let request = request(
        vec![message("hello".to_string())],
        "bounded instructions",
        /*text*/ None,
    );
    validate_model_request(
        &request,
        &[ToolSpec::Function(function("lookup", "small".to_string()))],
    )
    .expect("bounded model request");
}

#[test]
fn rejects_oversized_final_history_item() {
    let request = request(
        vec![message("x".repeat(max_model_request_item_bytes()))],
        "",
        /*text*/ None,
    );
    let error = validate_model_request(&request, &[])
        .expect_err("message envelope must remain below the item limit");

    assert!(error.to_string().contains("individual history item"));
}

#[test]
fn rejects_oversized_visible_summary_with_encrypted_reasoning() {
    let reasoning = ResponseItem::Reasoning {
        id: None,
        summary: vec![ReasoningItemReasoningSummary::SummaryText {
            text: "x".repeat(max_model_request_item_bytes()),
        }],
        content: None,
        encrypted_content: Some("encrypted".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };

    let error = validate_model_request(&request(vec![reasoning], "", /*text*/ None), &[])
        .expect_err("visible reasoning fields must remain below the item limit");

    assert!(error.to_string().contains("individual history item"));
}

#[test]
fn rejects_oversized_final_namespace_tool() {
    let description = "x".repeat(max_model_request_item_bytes() / 2);
    let namespace = ToolSpec::Namespace(ResponsesApiNamespace {
        name: "shared".to_string(),
        description: String::new(),
        tools: vec![
            ResponsesApiNamespaceTool::Function(function("one", description.clone())),
            ResponsesApiNamespaceTool::Function(function("two", description)),
        ],
    });

    let error = validate_model_request(&request(Vec::new(), "", /*text*/ None), &[namespace])
        .expect_err("merged namespace must remain below the item limit");

    assert!(error.to_string().contains("individual tool definition"));
}

#[test]
fn rejects_oversized_responses_lite_tools_envelope() {
    let input = ResponseItem::AdditionalTools {
        id: None,
        role: "developer".to_string(),
        tools: vec![json!({
            "type": "namespace",
            "name": "functions",
            "description": "x".repeat(max_model_request_item_bytes()),
            "tools": [],
        })],
    };

    let error = validate_model_request(&request(vec![input], "", /*text*/ None), &[])
        .expect_err("Responses Lite tools share one model-visible input item");

    assert!(error.to_string().contains("individual history item"));
}

#[test]
fn rejects_excessive_final_item_count() {
    let items = vec![message(String::new()); MAX_MODEL_REQUEST_ITEMS + 1];

    let error = validate_model_request(&request(items, "", /*text*/ None), &[])
        .expect_err("request cardinality must remain bounded");

    assert!(
        error
            .to_string()
            .contains("request exceeds the bounded context budget")
    );
}

#[test]
fn accepts_model_sized_image_despite_large_inline_payload() {
    let image = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputImage {
            image_url: format!(
                "data:image/png;base64,{}",
                "A".repeat(max_model_request_item_bytes() * 2)
            ),
            detail: None,
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    validate_model_request(&request(vec![image], "", /*text*/ None), &[])
        .expect("image payload uses its model-visible token estimate");
}

#[test]
fn rejects_oversized_output_schema() {
    let text = codex_api::create_text_param_for_request(
        /*verbosity*/ None,
        &Some(json!({
            "type": "object",
            "description": "x".repeat(max_model_request_item_bytes())
        })),
        /*output_schema_strict*/ true,
    );

    let error = validate_model_request(&request(Vec::new(), "", text), &[])
        .expect_err("output schema must remain below the item limit");

    assert!(error.to_string().contains("individual text controls item"));
}

#[test]
fn rejects_oversized_serialized_request() {
    let items = (0..500).map(|_| message("x".repeat(35_000))).collect();

    let error = validate_model_request(&request(items, "", /*text*/ None), &[])
        .expect_err("serialized request must remain below the total byte limit");

    assert!(
        error
            .to_string()
            .contains("request exceeds the bounded context budget")
    );
}
