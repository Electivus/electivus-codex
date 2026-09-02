use codex_api::ResponsesApiRequest;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ResponseItem;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::TruncationPolicy;
use serde::Serialize;
use std::collections::VecDeque;
use std::ops::Range;

use crate::context_manager::estimate_item_token_count;
use crate::context_manager::remove_corresponding_for;
use crate::context_manager::truncate_output_item_to_limit;
use crate::context_manager::updates::split_model_context_item_to_limit;

const MAX_MODEL_REQUEST_ITEMS: usize = 10_000;
const MAX_MODEL_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_MODEL_REQUEST_ITEM_TOKENS: usize = 10_000;

pub(crate) fn split_model_request_messages(
    request: &mut ResponsesApiRequest,
    tools: &[ToolSpec],
) -> Result<()> {
    request.input = split_input_items(std::mem::take(&mut request.input))?;
    validate_total_request_budget(request, tools)
}

pub(crate) fn bound_replayed_model_request(
    request: &mut ResponsesApiRequest,
    tools: &[ToolSpec],
    replay_range: Range<usize>,
) -> Result<()> {
    let replay_start = replay_range.start.min(request.input.len());
    let replay_end = replay_range.end.min(request.input.len()).max(replay_start);
    let suffix = split_input_items(request.input.split_off(replay_end))?;
    let replay = request.input.split_off(replay_start);
    request.input = split_input_items(std::mem::take(&mut request.input))?;
    let mut replay = replay
        .into_iter()
        .map(split_and_validate_input_item)
        .collect::<Result<VecDeque<_>>>()?;
    let replay_items = replay.iter().map(Vec::len).sum::<usize>();
    let flattened_replay = replay.iter().flatten().cloned().collect::<Vec<_>>();

    let mut item_count = request
        .input
        .len()
        .saturating_add(replay_items)
        .saturating_add(suffix.len())
        .saturating_add(tools.len())
        .saturating_add(usize::from(!request.instructions.is_empty()))
        .saturating_add(usize::from(request.text.is_some()));
    let mut input_item_count = request
        .input
        .len()
        .saturating_add(replay_items)
        .saturating_add(suffix.len());
    let mut model_visible_bytes = model_visible_request_bytes(request, &flattened_replay, &suffix)?;
    while item_count > MAX_MODEL_REQUEST_ITEMS || model_visible_bytes > MAX_MODEL_REQUEST_BYTES {
        let Some(removed_group) = replay.pop_front() else {
            return Err(limit_error("request exceeds the bounded context budget"));
        };
        subtract_input_items(
            &removed_group,
            &mut item_count,
            &mut input_item_count,
            &mut model_visible_bytes,
        );
        for removed in &removed_group {
            if let Some(corresponding) = remove_corresponding_from_groups(&mut replay, removed) {
                subtract_input_items(
                    std::slice::from_ref(&corresponding),
                    &mut item_count,
                    &mut input_item_count,
                    &mut model_visible_bytes,
                );
            }
        }
    }

    request.input.extend(replay.into_iter().flatten());
    request.input.extend(suffix);
    validate_total_request_budget(request, tools)
}

fn split_input_items(items: Vec<ResponseItem>) -> Result<Vec<ResponseItem>> {
    items
        .into_iter()
        .map(split_and_validate_input_item)
        .collect::<Result<Vec<_>>>()
        .map(|groups| groups.into_iter().flatten().collect())
}

fn split_and_validate_input_item(item: ResponseItem) -> Result<Vec<ResponseItem>> {
    let items = match item {
        ResponseItem::AdditionalTools { id, role, tools } => {
            let mut expanded_tools = Vec::new();
            let mut expanded_tools_bytes = 0usize;
            let mut push_expanded_tool = |tool: serde_json::Value| -> Result<()> {
                let tool_bytes = serde_json::to_vec(&tool)?.len();
                if expanded_tools.len() >= MAX_MODEL_REQUEST_ITEMS
                    || expanded_tools_bytes.saturating_add(tool_bytes) > MAX_MODEL_REQUEST_BYTES
                {
                    return Err(limit_error("request exceeds the bounded context budget"));
                }
                expanded_tools_bytes = expanded_tools_bytes.saturating_add(tool_bytes);
                expanded_tools.push(tool);
                Ok(())
            };
            for mut tool in tools {
                let single_tool_bytes = serde_json::to_vec(&ResponseItem::AdditionalTools {
                    id: id.clone(),
                    role: role.clone(),
                    tools: vec![tool.clone()],
                })?
                .len();
                if single_tool_bytes <= max_model_request_item_bytes()
                    || tool.get("type").and_then(serde_json::Value::as_str) != Some("namespace")
                {
                    push_expanded_tool(tool)?;
                    continue;
                }
                let Some(namespace_tools) = tool
                    .get_mut("tools")
                    .and_then(serde_json::Value::as_array_mut)
                    .map(std::mem::take)
                else {
                    push_expanded_tool(tool)?;
                    continue;
                };
                if namespace_tools.is_empty() {
                    push_expanded_tool(tool)?;
                    continue;
                }
                let namespace_template = tool;
                let empty_namespace_bytes = serde_json::to_vec(&ResponseItem::AdditionalTools {
                    id: id.clone(),
                    role: role.clone(),
                    tools: vec![namespace_template.clone()],
                })?
                .len();
                let mut fragment_tools = Vec::new();
                let mut fragment_bytes = empty_namespace_bytes;
                for namespace_tool in namespace_tools {
                    let namespace_tool_bytes = serde_json::to_vec(&namespace_tool)?.len();
                    let candidate_bytes = fragment_bytes
                        .saturating_add(namespace_tool_bytes)
                        .saturating_add(usize::from(!fragment_tools.is_empty()));
                    if candidate_bytes > max_model_request_item_bytes()
                        && !fragment_tools.is_empty()
                    {
                        let mut fragment = namespace_template.clone();
                        fragment["tools"] =
                            serde_json::Value::Array(std::mem::take(&mut fragment_tools));
                        push_expanded_tool(fragment)?;
                        fragment_bytes = empty_namespace_bytes;
                    }
                    fragment_bytes = fragment_bytes
                        .saturating_add(namespace_tool_bytes)
                        .saturating_add(usize::from(!fragment_tools.is_empty()));
                    fragment_tools.push(namespace_tool);
                }
                let mut fragment = namespace_template;
                fragment["tools"] = serde_json::Value::Array(fragment_tools);
                push_expanded_tool(fragment)?;
            }
            let mut fragments = Vec::new();
            let mut fragment_id = id;
            let mut fragment_tools = Vec::new();
            let mut fragment_bytes = serde_json::to_vec(&ResponseItem::AdditionalTools {
                id: fragment_id.clone(),
                role: role.clone(),
                tools: Vec::new(),
            })?
            .len();
            for tool in expanded_tools {
                let tool_bytes = serde_json::to_vec(&tool)?.len();
                let candidate_bytes = fragment_bytes
                    .saturating_add(tool_bytes)
                    .saturating_add(usize::from(!fragment_tools.is_empty()));
                if candidate_bytes > max_model_request_item_bytes() && !fragment_tools.is_empty() {
                    fragments.push(ResponseItem::AdditionalTools {
                        id: fragment_id.take(),
                        role: role.clone(),
                        tools: std::mem::take(&mut fragment_tools),
                    });
                    fragment_bytes = serde_json::to_vec(&ResponseItem::AdditionalTools {
                        id: None,
                        role: role.clone(),
                        tools: Vec::new(),
                    })?
                    .len();
                }
                fragment_bytes = fragment_bytes
                    .saturating_add(tool_bytes)
                    .saturating_add(usize::from(!fragment_tools.is_empty()));
                fragment_tools.push(tool);
            }
            if !fragment_tools.is_empty() || fragments.is_empty() {
                fragments.push(ResponseItem::AdditionalTools {
                    id: fragment_id,
                    role,
                    tools: fragment_tools,
                });
            }
            fragments
        }
        item => split_model_context_item_to_limit(item),
    };
    let mut bounded_items = Vec::with_capacity(items.len());
    for item in items {
        if estimate_item_token_count(&item) > MAX_MODEL_REQUEST_ITEM_TOKENS as i64
            && matches!(
                item,
                ResponseItem::Reasoning { .. }
                    | ResponseItem::Compaction { .. }
                    | ResponseItem::ContextCompaction { .. }
            )
        {
            continue;
        }
        let item = match &item {
            ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. } => {
                truncate_output_item_to_limit(&item)
                    .ok_or_else(|| item_limit_error("input item"))?
            }
            _ => item,
        };
        validate_response_item("input item", &item)?;
        bounded_items.push(item);
    }
    Ok(bounded_items)
}

fn validate_total_request_budget(request: &ResponsesApiRequest, tools: &[ToolSpec]) -> Result<()> {
    if !request.instructions.is_empty() {
        validate_serialized_item("instructions", &request.instructions)?;
    }
    for tool in tools {
        validate_serialized_item("tool definition", tool)?;
    }
    if let Some(text) = &request.text {
        validate_serialized_item("text controls", text)?;
    }
    if request_item_count(request, tools) > MAX_MODEL_REQUEST_ITEMS
        || model_visible_request_bytes(request, &[], &[])? > MAX_MODEL_REQUEST_BYTES
    {
        return Err(limit_error("request exceeds the bounded context budget"));
    }
    Ok(())
}

fn subtract_input_items(
    removed: &[ResponseItem],
    item_count: &mut usize,
    input_item_count: &mut usize,
    model_visible_bytes: &mut usize,
) {
    let removed_count = removed.len();
    let removed_separators = if *input_item_count > removed_count {
        removed_count
    } else {
        removed_count.saturating_sub(1)
    };
    let removed_bytes = removed
        .iter()
        .map(model_visible_item_bytes)
        .fold(removed_separators, usize::saturating_add);
    *item_count = item_count.saturating_sub(removed_count);
    *input_item_count = input_item_count.saturating_sub(removed_count);
    *model_visible_bytes = model_visible_bytes.saturating_sub(removed_bytes);
}

fn remove_corresponding_from_groups(
    groups: &mut VecDeque<Vec<ResponseItem>>,
    removed: &ResponseItem,
) -> Option<ResponseItem> {
    for index in 0..groups.len() {
        let corresponding = groups
            .get_mut(index)
            .and_then(|group| remove_corresponding_for(group, removed));
        if corresponding.is_some() {
            if groups.get(index).is_some_and(Vec::is_empty) {
                groups.remove(index);
            }
            return corresponding;
        }
    }
    None
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
