use super::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use codex_rollout::CompactedItem;
use pretty_assertions::assert_eq;

const TEST_WAV_SAMPLE_RATE: u32 = 8_000;

fn pcm_wav_data_url(sample_count: u32) -> String {
    let padding = sample_count % 2;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + sample_count + padding).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&TEST_WAV_SAMPLE_RATE.to_le_bytes());
    bytes.extend_from_slice(&TEST_WAV_SAMPLE_RATE.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&8u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&sample_count.to_le_bytes());
    bytes.resize(
        bytes.len() + sample_count as usize + padding as usize,
        /*value*/ 0,
    );
    format!("data:audio/wav;base64,{}", BASE64_STANDARD.encode(bytes))
}

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
    let discounted_audio_item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputAudio {
            audio_url: pcm_wav_data_url(/*sample_count*/ 40_000),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(
        serialized_bytes(&discounted_audio_item).expect("serialize discounted audio")
            > max_model_context_item_bytes()
    );
    let discounted_audio = RolloutItem::ResponseItem(discounted_audio_item.into());

    for accepted in [
        &presentation,
        &truncatable_output,
        &discounted_image,
        &discounted_audio,
    ] {
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
fn model_context_validation_accepts_replay_splittable_agent_messages() {
    let thread_id = codex_protocol::ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f038")
        .expect("thread id");
    let message = ResponseItem::AgentMessage {
        id: None,
        author: "parent".to_string(),
        recipient: "child".to_string(),
        content: vec![
            AgentMessageInputContent::InputText {
                text: "first".repeat(5_000),
            },
            AgentMessageInputContent::InputText {
                text: "second".repeat(5_000),
            },
        ],
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(
        serialized_bytes(&message).expect("serialize agent message")
            > max_model_context_item_bytes()
    );
    let mut budget = ModelContextBudget::new(thread_id);

    validate_response_item(&message, &mut budget)
        .expect("agent message should split during replay");

    assert_eq!(budget.items, 1);
    assert!(budget.bytes > 0);
}

#[test]
fn model_context_validation_packs_discounted_media_with_small_text() {
    let thread_id = codex_protocol::ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f038")
        .expect("thread id");
    let message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputImage {
                image_url: format!("data:image/png;base64,{}", "a".repeat(50_000)),
                detail: None,
            },
            ContentItem::InputText {
                text: "small suffix".to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(
        serialized_bytes(&message).expect("serialize mixed media message")
            > max_model_context_item_bytes()
    );
    let mut budget = ModelContextBudget::new(thread_id);

    validate_response_item(&message, &mut budget)
        .expect("discounted media and text should remain one projected item");

    assert_eq!((budget.items, budget.bytes), (0, 0));
}

#[test]
fn model_context_validation_accounts_multi_image_expansion_below_the_raw_limit() {
    let thread_id = codex_protocol::ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f038")
        .expect("thread id");
    let message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: (0..6)
            .map(|index| ContentItem::InputImage {
                image_url: format!("data:image/png;base64,image-{index}"),
                detail: None,
            })
            .collect(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(
        serialized_bytes(&message).expect("serialize multi-image message")
            < max_model_context_item_bytes()
    );
    assert!(model_visible_item_bytes(&message) > max_model_context_item_bytes());
    let mut budget = ModelContextBudget::new(thread_id);

    validate_response_item(&message, &mut budget)
        .expect("multi-image message should account its projected fragments");

    assert_eq!(budget.items, 1);
    assert!(budget.bytes > 0);
}

#[test]
fn model_context_validation_accepts_discounted_encrypted_agent_messages() {
    let thread_id = codex_protocol::ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f038")
        .expect("thread id");
    let message = ResponseItem::AgentMessage {
        id: None,
        author: "parent".to_string(),
        recipient: "child".to_string(),
        content: vec![AgentMessageInputContent::EncryptedContent {
            encrypted_content: "encrypted".repeat(6_000),
        }],
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(
        serialized_bytes(&message).expect("serialize encrypted agent message")
            > max_model_context_item_bytes()
    );
    let mut budget = ModelContextBudget::new(thread_id);

    validate_response_item(&message, &mut budget)
        .expect("encrypted agent message should use its discounted model size");

    assert_eq!((budget.items, budget.bytes), (0, 0));
}

#[test]
fn model_context_validation_rejects_discounted_payloads_that_still_exceed_the_limit() {
    let thread_id = codex_protocol::ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f038")
        .expect("thread id");
    let encrypted_message = ResponseItem::AgentMessage {
        id: None,
        author: "parent".to_string(),
        recipient: "child".to_string(),
        content: vec![AgentMessageInputContent::EncryptedContent {
            encrypted_content: "encrypted".repeat(10_000),
        }],
        internal_chat_message_metadata_passthrough: None,
    };
    let invalid_audio = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputAudio {
            audio_url: format!("data:audio/wav;base64,{}", "not-base64".repeat(5_000)),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let multi_image_output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-images".to_string(),
        output: FunctionCallOutputPayload::from_content_items(
            (0..6)
                .map(|index| FunctionCallOutputContentItem::InputImage {
                    image_url: format!("data:image/png;base64,image-{index}"),
                    detail: None,
                })
                .collect(),
        ),
        internal_chat_message_metadata_passthrough: None,
    };

    for rejected in [&encrypted_message, &invalid_audio, &multi_image_output] {
        let mut budget = ModelContextBudget::new(thread_id);
        let error = validate_response_item(rejected, &mut budget)
            .expect_err("discounted payload must actually fit the model item limit");
        assert!(
            error
                .to_string()
                .contains("individual model-visible history item")
        );
    }
}

#[test]
fn model_context_validation_packs_small_message_content_before_accounting_expansion() {
    let thread_id = codex_protocol::ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f038")
        .expect("thread id");
    let message = ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: (0..MAX_MODEL_CONTEXT_ITEMS)
            .map(|_| ContentItem::InputText {
                text: "x".to_string(),
            })
            .collect(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let mut budget = ModelContextBudget::new(thread_id);
    budget
        .account_item(/*item_bytes*/ 1)
        .expect("session metadata");
    budget.account_item(/*item_bytes*/ 1).expect("history row");

    validate_response_item(&message, &mut budget).expect("small content should pack during replay");

    assert!(budget.items < 100);
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
