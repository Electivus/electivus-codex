use codex_api::ResponsesApiRequest;
use codex_api::TextControls;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ResponseItem;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::TruncationPolicy;
use serde::Serialize;

use crate::context_manager::estimate_item_token_count;

const MAX_MODEL_REQUEST_ITEMS: usize = 10_000;
const MAX_MODEL_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_MODEL_REQUEST_ITEM_TOKENS: usize = 10_000;

struct ModelRequestBudget {
    items: usize,
}

impl ModelRequestBudget {
    fn new() -> Self {
        Self { items: 0 }
    }

    fn account_serialized(&mut self, kind: &str, item: &(impl Serialize + ?Sized)) -> Result<()> {
        let bytes = serde_json::to_vec(item)?.len();
        if bytes > max_model_request_item_bytes() {
            return Err(item_limit_error(kind));
        }
        self.account_item()
    }

    fn account_response_item(&mut self, item: &ResponseItem) -> Result<()> {
        if estimate_item_token_count(item) > MAX_MODEL_REQUEST_ITEM_TOKENS as i64 {
            return Err(item_limit_error("history item"));
        }
        self.account_item()
    }

    fn account_item(&mut self) -> Result<()> {
        let items = self
            .items
            .checked_add(1)
            .ok_or_else(|| limit_error("item count overflow"))?;
        if items > MAX_MODEL_REQUEST_ITEMS {
            return Err(limit_error("request exceeds the bounded context budget"));
        }
        self.items = items;
        Ok(())
    }
}

pub(crate) fn validate_model_request(
    request: &ResponsesApiRequest,
    tools: &[ToolSpec],
) -> Result<()> {
    let mut budget = ModelRequestBudget::new();
    if !request.instructions.is_empty() {
        budget.account_serialized("instructions item", &request.instructions)?;
    }
    for item in &request.input {
        budget.account_response_item(item)?;
    }
    for tool in tools {
        budget.account_serialized("tool definition", tool)?;
    }
    if let Some(text) = &request.text {
        account_text_controls(&mut budget, text)?;
    }
    let total_bytes = serde_json::to_vec(request)?.len();
    if total_bytes > MAX_MODEL_REQUEST_BYTES {
        return Err(limit_error("request exceeds the bounded context budget"));
    }
    Ok(())
}

fn account_text_controls(budget: &mut ModelRequestBudget, text: &TextControls) -> Result<()> {
    budget.account_serialized("text controls item", text)
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
