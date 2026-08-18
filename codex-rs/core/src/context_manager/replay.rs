use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_bytes_for_tokens;

use super::history::estimate_item_token_count;
use super::history::truncate_function_output_payload;
use super::updates::MAX_MODEL_CONTEXT_ITEM_TOKENS;

// Reserve stable headroom for live turn items, instructions, and tools added after reconstruction.
pub(super) const MAX_REPLAY_HISTORY_ITEMS: usize = 8_000;
pub(super) const MAX_REPLAY_HISTORY_BYTES: u64 = 12 * 1024 * 1024;

pub(crate) fn process_replayed_item(
    item: &ResponseItem,
    policy: TruncationPolicy,
) -> Option<ResponseItem> {
    match item {
        ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. } => {
            truncate_replayed_output_item(item, policy * 1.2)
        }
        ResponseItem::Message { .. } | ResponseItem::AgentMessage { .. } => Some(item.clone()),
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::Other => {
            (estimate_item_token_count(item) <= MAX_MODEL_CONTEXT_ITEM_TOKENS).then(|| item.clone())
        }
    }
}

pub(super) fn truncate_output_item(item: &ResponseItem, policy: TruncationPolicy) -> ResponseItem {
    match item {
        ResponseItem::FunctionCallOutput {
            id,
            call_id,
            output,
            internal_chat_message_metadata_passthrough: metadata,
        } => ResponseItem::FunctionCallOutput {
            id: id.clone(),
            call_id: call_id.clone(),
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

fn truncate_replayed_output_item(
    item: &ResponseItem,
    preferred_policy: TruncationPolicy,
) -> Option<ResponseItem> {
    let preferred = truncate_output_item(item, preferred_policy);
    if estimate_item_token_count(&preferred) <= MAX_MODEL_CONTEXT_ITEM_TOKENS {
        return Some(preferred);
    }

    let max_budget = match preferred_policy {
        TruncationPolicy::Bytes(bytes) => bytes.min(approx_bytes_for_tokens(
            usize::try_from(MAX_MODEL_CONTEXT_ITEM_TOKENS).unwrap_or(usize::MAX),
        )),
        TruncationPolicy::Tokens(tokens) => {
            tokens.min(usize::try_from(MAX_MODEL_CONTEXT_ITEM_TOKENS).unwrap_or(usize::MAX))
        }
    };
    let mut lower = 0;
    let mut upper = max_budget;
    let mut best = None;
    while lower <= upper {
        let budget = lower + (upper - lower) / 2;
        let policy = match preferred_policy {
            TruncationPolicy::Bytes(_) => TruncationPolicy::Bytes(budget),
            TruncationPolicy::Tokens(_) => TruncationPolicy::Tokens(budget),
        };
        let candidate = truncate_output_item(item, policy);
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
