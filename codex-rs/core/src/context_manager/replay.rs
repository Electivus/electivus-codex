use codex_history::CodexHarnessMetadata;
use codex_model_context::estimate_response_item_model_visible_bytes;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::TruncationPolicy;

use super::history::estimate_item_token_count;
use super::history::truncate_function_output_payload;
use super::updates::MAX_MODEL_CONTEXT_ITEM_TOKENS;

// Reserve stable headroom for live turn items, instructions, and tools added after reconstruction.
pub(super) const MAX_REPLAY_HISTORY_ITEMS: usize = 8_000;
pub(super) const MAX_REPLAY_HISTORY_BYTES: u64 = 12 * 1024 * 1024;
const STRUCTURED_TOOL_OUTPUT_OMISSION_NOTICE: &str =
    "[omitted structured tool output content to fit model context limit]";
const STRUCTURED_OUTPUT_PROJECTION_HEADROOM_BYTES: i64 = 1_024;

pub(crate) fn process_replayed_item(
    item: &ResponseItem,
    policy: TruncationPolicy,
) -> Option<ResponseItem> {
    process_replayed_item_with_output_policy(item, policy * 1.2)
}

pub(crate) fn process_replayed_annotated_item(
    item: &ResponseItem,
    metadata: Option<&CodexHarnessMetadata>,
    policy: TruncationPolicy,
) -> Option<ResponseItem> {
    let output_policy = metadata
        .and_then(|metadata| metadata.fallback_token_limit_override)
        .map(TruncationPolicy::Tokens)
        .unwrap_or(policy * 1.2);
    process_replayed_item_with_output_policy(item, output_policy)
}

fn process_replayed_item_with_output_policy(
    item: &ResponseItem,
    output_policy: TruncationPolicy,
) -> Option<ResponseItem> {
    match item {
        ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. } => {
            let preferred = truncate_output_item(item, output_policy);
            truncate_output_item_to_limit(&preferred)
        }
        ResponseItem::Message { .. } | ResponseItem::AgentMessage { .. } => Some(item.clone()),
        ResponseItem::Reasoning { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. } => Some(item.clone()),
        ResponseItem::FunctionCall { .. } | ResponseItem::CustomToolCall { .. } => {
            let projected = project_tool_call_input_to_limit(item);
            (estimate_item_token_count(&projected) <= MAX_MODEL_CONTEXT_ITEM_TOKENS)
                .then_some(projected)
        }
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::Other => {
            (estimate_item_token_count(item) <= MAX_MODEL_CONTEXT_ITEM_TOKENS).then(|| item.clone())
        }
    }
}

pub(crate) fn project_tool_call_input_to_limit(item: &ResponseItem) -> ResponseItem {
    if estimate_item_token_count(item) <= MAX_MODEL_CONTEXT_ITEM_TOKENS {
        return item.clone();
    }

    let mut projected = item.clone();
    match &mut projected {
        ResponseItem::FunctionCall {
            arguments,
            encrypted_function_args,
            ..
        } => {
            let original_argument_bytes = arguments.len();
            let encrypted_argument_bytes = encrypted_function_args
                .as_ref()
                .map(|arguments| arguments.iter().map(String::len).sum::<usize>())
                .unwrap_or_default();
            *arguments = serde_json::json!({
                "_codex_tool_call_input_omitted": {
                    "original_argument_bytes": original_argument_bytes,
                    "encrypted_argument_bytes": encrypted_argument_bytes,
                }
            })
            .to_string();
            *encrypted_function_args = None;
        }
        ResponseItem::CustomToolCall { input, .. } => {
            let original_bytes = input.len();
            *input = format!(
                "[tool call input omitted to fit model context limit; original bytes: {original_bytes}]"
            );
        }
        _ => {}
    }
    projected
}

pub(super) fn truncate_output_item(item: &ResponseItem, policy: TruncationPolicy) -> ResponseItem {
    match item {
        ResponseItem::FunctionCallOutput {
            id,
            call_id,
            name,
            namespace,
            output,
            internal_chat_message_metadata_passthrough: metadata,
        } => ResponseItem::FunctionCallOutput {
            id: id.clone(),
            call_id: call_id.clone(),
            name: name.clone(),
            namespace: namespace.clone(),
            output: truncate_function_output_payload(output, policy),
            internal_chat_message_metadata_passthrough: metadata.clone(),
        },
        ResponseItem::CustomToolCallOutput {
            id,
            call_id,
            name,
            output,
            internal_chat_message_metadata_passthrough: metadata,
        } => ResponseItem::CustomToolCallOutput {
            id: id.clone(),
            call_id: call_id.clone(),
            name: name.clone(),
            output: truncate_function_output_payload(output, policy),
            internal_chat_message_metadata_passthrough: metadata.clone(),
        },
        _ => item.clone(),
    }
}

pub(crate) fn truncate_output_item_to_limit(item: &ResponseItem) -> Option<ResponseItem> {
    if estimate_item_token_count(item) <= MAX_MODEL_CONTEXT_ITEM_TOKENS {
        return Some(item.clone());
    }

    truncate_output_text_to_limit(item).or_else(|| {
        let projected = project_structured_output_fixed_content(item)?;
        truncate_output_text_to_limit(&projected).or_else(|| {
            let empty = with_structured_output_content(item, Vec::new())?;
            (estimate_item_token_count(&empty) <= MAX_MODEL_CONTEXT_ITEM_TOKENS).then_some(empty)
        })
    })
}

fn truncate_output_text_to_limit(item: &ResponseItem) -> Option<ResponseItem> {
    // Use one canonical token projection for both live and replayed history once the supplied
    // item exceeds the hard cap. The caller applies any model-specific policy before this helper.
    let max_budget = usize::try_from(MAX_MODEL_CONTEXT_ITEM_TOKENS).unwrap_or(usize::MAX);
    let mut lower = 0;
    let mut upper = max_budget;
    let mut best = None;
    while lower <= upper {
        let budget = lower + (upper - lower) / 2;
        let candidate = truncate_output_item(item, TruncationPolicy::Tokens(budget));
        if estimate_item_token_count(&candidate) <= MAX_MODEL_CONTEXT_ITEM_TOKENS {
            best = Some(candidate);
            lower = budget.saturating_add(1);
        } else if budget == 0 {
            break;
        } else {
            upper = budget - 1;
        }
    }
    best
}

fn project_structured_output_fixed_content(item: &ResponseItem) -> Option<ResponseItem> {
    let original_content = match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output.content_items()?,
        _ => return None,
    };
    let notice = FunctionCallOutputContentItem::InputText {
        text: STRUCTURED_TOOL_OUTPUT_OMISSION_NOTICE.to_string(),
    };
    let empty = with_structured_output_content(item, Vec::new())?;
    let empty_bytes = estimate_response_item_model_visible_bytes(&empty);
    let notice_only = with_structured_output_content(item, vec![notice.clone()])?;
    let max_bytes = i64::try_from(
        TruncationPolicy::Tokens(
            usize::try_from(MAX_MODEL_CONTEXT_ITEM_TOKENS).unwrap_or(usize::MAX),
        )
        .byte_budget(),
    )
    .unwrap_or(i64::MAX);
    let mut remaining_fixed_bytes = max_bytes
        .saturating_sub(STRUCTURED_OUTPUT_PROJECTION_HEADROOM_BYTES)
        .saturating_sub(estimate_response_item_model_visible_bytes(&notice_only));
    let mut projected_content = Vec::with_capacity(original_content.len().saturating_add(1));
    projected_content.push(notice);
    let mut omitted_fixed_content = false;

    for content_item in original_content {
        match content_item {
            FunctionCallOutputContentItem::InputText { .. }
            | FunctionCallOutputContentItem::InputAudio { .. } => {
                projected_content.push(content_item.clone());
            }
            FunctionCallOutputContentItem::InputImage { .. }
            | FunctionCallOutputContentItem::EncryptedContent { .. } => {
                if omitted_fixed_content {
                    continue;
                }
                let single = with_structured_output_content(&empty, vec![content_item.clone()])?;
                let contribution = estimate_response_item_model_visible_bytes(&single)
                    .saturating_sub(empty_bytes)
                    .saturating_add(1);
                if contribution <= remaining_fixed_bytes {
                    projected_content.push(content_item.clone());
                    remaining_fixed_bytes = remaining_fixed_bytes.saturating_sub(contribution);
                } else {
                    omitted_fixed_content = true;
                }
            }
        }
    }

    if omitted_fixed_content {
        with_structured_output_content(item, projected_content)
    } else {
        None
    }
}

fn with_structured_output_content(
    item: &ResponseItem,
    content: Vec<FunctionCallOutputContentItem>,
) -> Option<ResponseItem> {
    let mut projected = item.clone();
    let output = match &mut projected {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output,
        _ => return None,
    };
    if !matches!(output.body, FunctionCallOutputBody::ContentItems(_)) {
        return None;
    }
    output.body = FunctionCallOutputBody::ContentItems(content);
    Some(projected)
}
