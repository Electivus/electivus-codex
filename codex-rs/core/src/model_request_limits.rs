use codex_api::ResponsesApiRequest;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ResponseItem;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::TruncationPolicy;
use serde::Serialize;
use std::ops::Range;

use crate::context_manager::estimate_item_token_count;
use crate::context_manager::remove_corresponding_for;

const MAX_MODEL_REQUEST_ITEMS: usize = 10_000;
const MAX_MODEL_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_MODEL_REQUEST_ITEM_TOKENS: usize = 10_000;

pub(crate) fn bound_replayed_model_request(
    request: &mut ResponsesApiRequest,
    tools: &[ToolSpec],
    replay_range: Range<usize>,
    validate_replayed_tools: bool,
) -> Result<()> {
    if validate_replayed_tools {
        if tools.is_empty() {
            if let Some(additional_tools) = request
                .input
                .iter()
                .find(|item| matches!(item, ResponseItem::AdditionalTools { .. }))
            {
                validate_response_item("tools item", additional_tools)?;
            }
        } else {
            for tool in tools {
                validate_serialized_item("tool definition", tool)?;
            }
        }
    }

    let replay_start = replay_range.start.min(request.input.len());
    let replay_end = replay_range.end.min(request.input.len()).max(replay_start);
    let mut suffix = request.input.split_off(replay_end);
    let mut replay = request.input.split_off(replay_start);

    let mut item_count = request
        .input
        .len()
        .saturating_add(replay.len())
        .saturating_add(suffix.len())
        .saturating_add(tools.len())
        .saturating_add(usize::from(!request.instructions.is_empty()))
        .saturating_add(usize::from(request.text.is_some()));
    let mut input_item_count = request
        .input
        .len()
        .saturating_add(replay.len())
        .saturating_add(suffix.len());
    let mut model_visible_bytes = model_visible_request_bytes(request, &replay, &suffix)?;
    while item_count > MAX_MODEL_REQUEST_ITEMS || model_visible_bytes > MAX_MODEL_REQUEST_BYTES {
        if replay.is_empty() {
            return Err(limit_error("request exceeds the bounded context budget"));
        }
        let removed = replay.remove(0);
        item_count = item_count.saturating_sub(1);
        model_visible_bytes = model_visible_bytes
            .saturating_sub(model_visible_item_bytes(&removed) + usize::from(input_item_count > 1));
        input_item_count = input_item_count.saturating_sub(1);
        if let Some(corresponding) = remove_corresponding_for(&mut replay, &removed) {
            item_count = item_count.saturating_sub(1);
            model_visible_bytes = model_visible_bytes.saturating_sub(
                model_visible_item_bytes(&corresponding) + usize::from(input_item_count > 1),
            );
            input_item_count = input_item_count.saturating_sub(1);
        }
    }

    request.input.append(&mut replay);
    request.input.append(&mut suffix);
    if request_item_count(request, tools) > MAX_MODEL_REQUEST_ITEMS
        || model_visible_request_bytes(request, &[], &[])? > MAX_MODEL_REQUEST_BYTES
    {
        return Err(limit_error("request exceeds the bounded context budget"));
    }
    Ok(())
}

fn validate_response_item(kind: &str, item: &ResponseItem) -> Result<()> {
    if estimate_item_token_count(item) > MAX_MODEL_REQUEST_ITEM_TOKENS as i64 {
        return Err(item_limit_error(kind));
    }
    Ok(())
}

fn validate_serialized_item(kind: &str, item: &(impl Serialize + ?Sized)) -> Result<()> {
    if serde_json::to_vec(item)?.len() > max_model_request_item_bytes() {
        return Err(item_limit_error(kind));
    }
    Ok(())
}

fn model_visible_request_bytes(
    request: &ResponsesApiRequest,
    replay: &[ResponseItem],
    suffix: &[ResponseItem],
) -> Result<usize> {
    let input_items = request.input.iter().chain(replay).chain(suffix);
    let input_item_count = input_items.clone().count();
    let mut model_visible_bytes = b"{\"input\":[]}".len();
    model_visible_bytes = model_visible_bytes.saturating_add(
        input_items
            .map(model_visible_item_bytes)
            .fold(0usize, usize::saturating_add),
    );
    model_visible_bytes = model_visible_bytes.saturating_add(input_item_count.saturating_sub(1));
    if !request.instructions.is_empty() {
        model_visible_bytes = model_visible_bytes
            .saturating_add(b",\"instructions\":".len())
            .saturating_add(serde_json::to_vec(&request.instructions)?.len());
    }
    if let Some(tools) = request.tools.as_ref() {
        model_visible_bytes = model_visible_bytes
            .saturating_add(b",\"tools\":".len())
            .saturating_add(serde_json::to_vec(tools)?.len());
    }
    if let Some(text) = request.text.as_ref() {
        model_visible_bytes = model_visible_bytes
            .saturating_add(b",\"text\":".len())
            .saturating_add(serde_json::to_vec(text)?.len());
    }
    Ok(model_visible_bytes)
}

fn model_visible_item_bytes(item: &ResponseItem) -> usize {
    usize::try_from(estimate_item_token_count(item))
        .unwrap_or(usize::MAX)
        .saturating_mul(4)
}

fn request_item_count(request: &ResponsesApiRequest, tools: &[ToolSpec]) -> usize {
    request
        .input
        .len()
        .saturating_add(tools.len())
        .saturating_add(usize::from(!request.instructions.is_empty()))
        .saturating_add(usize::from(request.text.is_some()))
}

fn max_model_request_item_bytes() -> usize {
    TruncationPolicy::Tokens(MAX_MODEL_REQUEST_ITEM_TOKENS).byte_budget()
}

fn limit_error(reason: &str) -> CodexErr {
    CodexErr::InvalidRequest(format!(
        "model request cannot be built safely: {reason} (limit: {MAX_MODEL_REQUEST_ITEMS} items or {MAX_MODEL_REQUEST_BYTES} bytes)"
    ))
}

fn item_limit_error(kind: &str) -> CodexErr {
    CodexErr::InvalidRequest(format!(
        "model request cannot be built safely: an individual {kind} exceeds {MAX_MODEL_REQUEST_ITEM_TOKENS} estimated tokens"
    ))
}

#[cfg(test)]
#[path = "model_request_limits_tests.rs"]
mod tests;
