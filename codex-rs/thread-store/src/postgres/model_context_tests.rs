use super::*;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use codex_rollout::CompactedItem;
use pretty_assertions::assert_eq;

#[test]
fn model_context_budget_rejects_unbounded_item_counts_and_bytes() {
    let thread_id = codex_protocol::ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f038")
        .expect("thread id");
    let mut item_limited =
        ModelContextBudget::with_limits(thread_id, /*max_items*/ 2, /*max_bytes*/ 100);
    item_limited
        .account_item(/*item_bytes*/ 10)
        .expect("first item");
    item_limited
        .account_item(/*item_bytes*/ 10)
        .expect("second item");
    let item_error = item_limited
        .account_item(/*item_bytes*/ 10)
        .expect_err("third item");

    let mut byte_limited =
        ModelContextBudget::with_limits(thread_id, /*max_items*/ 10, /*max_bytes*/ 20);
    byte_limited
        .account_item(/*item_bytes*/ 20)
        .expect("exact byte budget");
    let byte_error = byte_limited
        .account_item(/*item_bytes*/ 1)
        .expect_err("byte budget overflow");

    assert_eq!(
        [item_error.to_string(), byte_error.to_string()],
        [
            format!(
                "invalid thread-store request: model context for thread {thread_id} cannot be loaded safely: history exceeds the bounded read budget (limit: 2 items or 100 bytes)"
            ),
            format!(
                "invalid thread-store request: model context for thread {thread_id} cannot be loaded safely: history exceeds the bounded read budget (limit: 10 items or 20 bytes)"
            ),
        ]
    );
}

#[test]
fn model_context_validation_distinguishes_replay_safe_and_rejected_items() {
    let thread_id = codex_protocol::ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f038")
        .expect("thread id");
    let presentation = RolloutItem::EventMsg(EventMsg::Warning(WarningEvent {
        message: "presentation".repeat(50_000),
    }));
    let oversized_message = RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "model-visible".repeat(50_000),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    );
    let truncatable_output = RolloutItem::ResponseItem(
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_text("output".repeat(50_000)),
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    );
    let impossible_output = RolloutItem::ResponseItem(
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call".repeat(50_000),
            output: FunctionCallOutputPayload::from_text("output".repeat(50_000)),
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    );
    let discounted_image = RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: format!("data:image/png;base64,{}", "a".repeat(50_000)),
                detail: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    );

    for accepted in [&presentation, &truncatable_output, &discounted_image] {
        let mut budget = ModelContextBudget::new(thread_id);
        validate_model_context_item(accepted, &mut budget).expect("item should be accepted");
    }
    let mut budget = ModelContextBudget::new(thread_id);
    validate_model_context_item(&oversized_message, &mut budget)
        .expect("splittable message should be accepted");

    let mut budget = ModelContextBudget::new(thread_id);
    let error = validate_model_context_item(&impossible_output, &mut budget)
        .expect_err("model-visible item should be rejected");
    assert!(
        error
            .to_string()
            .contains("individual model-visible history item")
    );
}

#[test]
fn model_context_validation_accepts_replay_splittable_messages_and_accounts_expansion() {
    let thread_id = codex_protocol::ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f038")
        .expect("thread id");
    let message = ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "memory".repeat(4_000),
            },
            ContentItem::InputText {
                text: "skills".repeat(4_000),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(
        serialized_bytes(&message).expect("serialize message") > max_model_context_item_bytes()
    );
    let mut budget = ModelContextBudget::new(thread_id);
    validate_response_item(&message, &mut budget).expect("message should split during replay");
    assert_eq!(budget.items, 1);
    assert!(budget.bytes > 0);
}

#[test]
fn compacted_replacement_history_is_bounded_after_expansion() {
    let thread_id = codex_protocol::ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f038")
        .expect("thread id");
    let replacement = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "bounded".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let compacted = RolloutItem::Compacted(CompactedItem {
        message: "summary".to_string(),
        replacement_history: Some(vec![replacement.into(); MAX_MODEL_CONTEXT_ITEMS + 1]),
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    });
    let mut budget = ModelContextBudget::new(thread_id);
    budget
        .account_item(/*item_bytes*/ 1)
        .expect("compacted row");

    let error = validate_model_context_item(&compacted, &mut budget)
        .expect_err("expanded replacement history must exceed the item budget");

    assert!(
        error
            .to_string()
            .contains("history exceeds the bounded read budget")
    );
}

#[test]
fn session_metadata_limits_use_model_visible_instruction_and_tool_envelopes() {
    let thread_id = codex_protocol::ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f038")
        .expect("thread id");
    let mut budget = ModelContextBudget::new(thread_id);
    let mut instruction_length = max_model_context_item_bytes();
    let base_instructions = loop {
        let candidate = BaseInstructions {
            text: "x".repeat(instruction_length),
            provenance: None,
        };
        if serialized_bytes(&candidate).expect("serialize base instructions")
            <= max_model_context_item_bytes()
        {
            break candidate;
        }
        instruction_length -= 1;
    };
    let instructions_error = validate_base_instructions(&base_instructions, &budget)
        .expect_err("Responses Lite message envelope must count toward the item limit");

    let direct_tools = (0..500)
        .map(|index| {
            DynamicToolSpec::Function(DynamicToolFunctionSpec {
                name: format!("direct_{index}"),
                description: "model-visible dynamic tool".to_string(),
                input_schema: serde_json::json!({ "type": "object" }),
                defer_loading: false,
            })
        })
        .collect::<Vec<_>>();
    let direct_error = validate_dynamic_tools(&direct_tools, &mut budget)
        .expect_err("Responses Lite must bound its aggregated AdditionalTools item");

    let deferred_tools = (0..500)
        .map(|index| {
            DynamicToolSpec::Function(DynamicToolFunctionSpec {
                name: format!("deferred_{index}"),
                description: "model-visible deferred dynamic tool".to_string(),
                input_schema: serde_json::json!({ "type": "object" }),
                defer_loading: true,
            })
        })
        .collect::<Vec<_>>();
    let deferred_error = validate_dynamic_tools(&deferred_tools, &mut budget)
        .expect_err("tool_search output must be bounded after coalescing deferred tools");

    for error in [instructions_error, direct_error, deferred_error] {
        assert!(
            error
                .to_string()
                .contains("individual model-visible history item")
        );
    }
}
