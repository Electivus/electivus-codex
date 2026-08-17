use std::collections::BTreeMap;

use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TruncationPolicy;
use codex_rollout::ModelContextScan;
use codex_rollout::ModelContextScanProgress;
use codex_rollout::RolloutItem;
use codex_tools::LoadableToolSpec;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use codex_tools::coalesce_loadable_tool_specs;
use codex_tools::create_tools_json_for_responses_lite;
use codex_tools::default_namespace_description;
use codex_tools::dynamic_tool_to_responses_api_tool;
use futures::TryStreamExt;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::PostgresThreadStore;
use super::database_error;
use super::serialization_error;
use crate::LoadThreadHistoryParams;
use crate::StoredModelContext;
use crate::StoredThread;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

const MAX_MODEL_CONTEXT_ITEMS: usize = 10_000;
const MAX_MODEL_CONTEXT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MODEL_CONTEXT_ITEM_TOKENS: usize = 10_000;

fn max_model_context_item_bytes() -> usize {
    TruncationPolicy::Tokens(MAX_MODEL_CONTEXT_ITEM_TOKENS).byte_budget()
}

struct ModelContextBudget {
    thread_id: codex_protocol::ThreadId,
    items: usize,
    bytes: u64,
    max_items: usize,
    max_bytes: u64,
}

impl ModelContextBudget {
    fn new(thread_id: codex_protocol::ThreadId) -> Self {
        Self::with_limits(thread_id, MAX_MODEL_CONTEXT_ITEMS, MAX_MODEL_CONTEXT_BYTES)
    }

    fn with_limits(thread_id: codex_protocol::ThreadId, max_items: usize, max_bytes: u64) -> Self {
        Self {
            thread_id,
            items: 0,
            bytes: 0,
            max_items,
            max_bytes,
        }
    }

    fn account_item(&mut self, item_bytes: i64) -> ThreadStoreResult<()> {
        let item_bytes =
            u64::try_from(item_bytes).map_err(|_| self.limit_error("invalid item size"))?;
        let next_items = self
            .items
            .checked_add(1)
            .ok_or_else(|| self.limit_error("item count overflow"))?;
        let next_bytes = self
            .bytes
            .checked_add(item_bytes)
            .ok_or_else(|| self.limit_error("byte count overflow"))?;
        if next_items > self.max_items || next_bytes > self.max_bytes {
            return Err(self.limit_error("history exceeds the bounded read budget"));
        }
        self.items = next_items;
        self.bytes = next_bytes;
        Ok(())
    }

    fn account_expanded_items(&mut self, additional_items: usize) -> ThreadStoreResult<()> {
        self.account_expansion(additional_items, /*additional_bytes*/ 0)
    }

    fn account_expansion(
        &mut self,
        additional_items: usize,
        additional_bytes: usize,
    ) -> ThreadStoreResult<()> {
        let next_items = self
            .items
            .checked_add(additional_items)
            .ok_or_else(|| self.limit_error("item count overflow"))?;
        let additional_bytes = u64::try_from(additional_bytes)
            .map_err(|_| self.limit_error("invalid expansion size"))?;
        let next_bytes = self
            .bytes
            .checked_add(additional_bytes)
            .ok_or_else(|| self.limit_error("byte count overflow"))?;
        if next_items > self.max_items || next_bytes > self.max_bytes {
            return Err(self.limit_error("history exceeds the bounded read budget"));
        }
        self.items = next_items;
        self.bytes = next_bytes;
        Ok(())
    }

    fn limit_error(&self, reason: &str) -> ThreadStoreError {
        ThreadStoreError::InvalidRequest {
            message: format!(
                "model context for thread {} cannot be loaded safely: {reason} (limit: {} items or {} bytes)",
                self.thread_id, self.max_items, self.max_bytes
            ),
        }
    }
}

fn bounded_item_from_row(
    row: &PgRow,
    budget: &ModelContextBudget,
) -> ThreadStoreResult<RolloutItem> {
    let item: Option<Value> = row
        .try_get("item")
        .map_err(|error| database_error("load latest model context", error))?;
    let item = item.ok_or_else(|| budget.limit_error("an individual history item is too large"))?;
    serde_json::from_value(item).map_err(serialization_error)
}

fn validate_model_context_item(
    item: &RolloutItem,
    budget: &mut ModelContextBudget,
) -> ThreadStoreResult<()> {
    match item {
        RolloutItem::SessionMeta(meta) => {
            if let Some(base_instructions) = &meta.meta.base_instructions {
                validate_base_instructions(base_instructions, budget)?;
            }
            validate_dynamic_tools(
                meta.meta.dynamic_tools.as_deref().unwrap_or_default(),
                budget,
            )?;
            let expanded_tools = meta
                .meta
                .dynamic_tools
                .as_deref()
                .unwrap_or_default()
                .iter()
                .try_fold(0usize, |count, tool| {
                    let tool_count = match tool {
                        DynamicToolSpec::Function(_) => 1,
                        DynamicToolSpec::Namespace(namespace) => namespace.tools.len(),
                    };
                    count.checked_add(tool_count)
                })
                .ok_or_else(|| budget.limit_error("item count overflow"))?;
            budget.account_expanded_items(expanded_tools)
        }
        RolloutItem::ResponseItem(item) => validate_response_item(item, budget),
        RolloutItem::InterAgentCommunication(communication) => {
            validate_response_item(&communication.to_model_input_item(), budget)
        }
        RolloutItem::Compacted(compacted) => {
            if let Some(replacement_history) = &compacted.replacement_history {
                budget.account_expanded_items(replacement_history.len().saturating_sub(1))?;
                for item in replacement_history {
                    validate_response_item(item, budget)?;
                }
            } else {
                validate_response_item(&ResponseItem::from(compacted.clone()), budget)?;
            }
            Ok(())
        }
        RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::TurnContext(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::SecurityRiskScore(_)
        | RolloutItem::EventMsg(_) => Ok(()),
    }
}

fn validate_response_item(
    item: &ResponseItem,
    budget: &mut ModelContextBudget,
) -> ThreadStoreResult<()> {
    let item_bytes = serialized_bytes(item)?;
    if item_bytes <= max_model_context_item_bytes() {
        return Ok(());
    }
    if let Some(expansion) = replay_splittable_message_expansion(item, item_bytes, budget)? {
        budget.account_expansion(expansion.additional_items, expansion.additional_bytes)?;
        return Ok(());
    }
    if has_discounted_model_payload(item) || replay_truncatable_output_minimum_fits(item, budget)? {
        return Ok(());
    }
    Err(budget.limit_error(&format!(
        "an individual model-visible history item exceeds {MAX_MODEL_CONTEXT_ITEM_TOKENS} estimated tokens"
    )))
}

fn validate_serialized_model_item(
    item: &(impl serde::Serialize + ?Sized),
    budget: &ModelContextBudget,
) -> ThreadStoreResult<()> {
    if serialized_bytes(item)? <= max_model_context_item_bytes() {
        return Ok(());
    }
    Err(budget.limit_error(&format!(
        "an individual model-visible history item exceeds {MAX_MODEL_CONTEXT_ITEM_TOKENS} estimated tokens"
    )))
}

fn validate_base_instructions(
    base_instructions: &codex_protocol::models::BaseInstructions,
    budget: &ModelContextBudget,
) -> ThreadStoreResult<()> {
    validate_serialized_model_item(
        &ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: base_instructions.text.clone(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        budget,
    )
}

fn validate_dynamic_tools(
    tools: &[DynamicToolSpec],
    budget: &mut ModelContextBudget,
) -> ThreadStoreResult<()> {
    let mut direct_specs = Vec::new();
    let mut direct_namespaces = BTreeMap::<String, ResponsesApiNamespace>::new();
    let mut deferred_specs = Vec::new();
    for spec in tools {
        match spec {
            DynamicToolSpec::Function(function) => append_dynamic_tool(
                function,
                /*namespace*/ None,
                &mut direct_specs,
                &mut direct_namespaces,
                &mut deferred_specs,
            ),
            DynamicToolSpec::Namespace(namespace) => {
                for tool in &namespace.tools {
                    let codex_protocol::dynamic_tools::DynamicToolNamespaceTool::Function(function) =
                        tool;
                    append_dynamic_tool(
                        function,
                        Some((namespace.name.as_str(), namespace.description.as_str())),
                        &mut direct_specs,
                        &mut direct_namespaces,
                        &mut deferred_specs,
                    );
                }
            }
        }
    }

    direct_specs.extend(direct_namespaces.into_values().map(ToolSpec::Namespace));
    for spec in &direct_specs {
        validate_serialized_model_item(spec, budget)?;
    }
    let direct_tools =
        create_tools_json_for_responses_lite(&direct_specs).map_err(serialization_error)?;
    if !direct_tools.is_empty() {
        validate_response_item(
            &ResponseItem::AdditionalTools {
                id: None,
                role: "developer".to_string(),
                tools: direct_tools,
            },
            budget,
        )?;
    }

    let deferred_tools = coalesce_loadable_tool_specs(deferred_specs)
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(serialization_error)?;
    if !deferred_tools.is_empty() {
        validate_response_item(
            &ResponseItem::ToolSearchOutput {
                id: None,
                call_id: Some(String::new()),
                status: "completed".to_string(),
                execution: "client".to_string(),
                tools: deferred_tools,
                internal_chat_message_metadata_passthrough: None,
            },
            budget,
        )?;
    }
    Ok(())
}

fn append_dynamic_tool(
    function: &codex_protocol::dynamic_tools::DynamicToolFunctionSpec,
    namespace: Option<(&str, &str)>,
    direct_specs: &mut Vec<ToolSpec>,
    direct_namespaces: &mut BTreeMap<String, ResponsesApiNamespace>,
    deferred_specs: &mut Vec<LoadableToolSpec>,
) {
    let Ok(mut tool) = dynamic_tool_to_responses_api_tool(function) else {
        return;
    };
    tool.defer_loading = None;
    let spec = if let Some((name, description)) = namespace {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: name.to_string(),
            description: if description.trim().is_empty() {
                default_namespace_description(name)
            } else {
                description.to_string()
            },
            tools: vec![ResponsesApiNamespaceTool::Function(tool)],
        })
    } else {
        ToolSpec::Function(tool)
    };
    if function.defer_loading {
        if let Some(search_info) = ToolSearchInfo::from_tool_spec(spec, /*source_info*/ None) {
            deferred_specs.push(search_info.entry.output);
        }
    } else if let ToolSpec::Namespace(mut namespace) = spec {
        let existing = direct_namespaces
            .entry(namespace.name.clone())
            .or_insert_with(|| ResponsesApiNamespace {
                name: namespace.name.clone(),
                description: namespace.description.clone(),
                tools: Vec::new(),
            });
        if existing.description.trim().is_empty() && !namespace.description.trim().is_empty() {
            existing.description = namespace.description;
        }
        existing.tools.append(&mut namespace.tools);
    } else {
        direct_specs.push(spec);
    }
}

struct ReplayMessageExpansion {
    additional_items: usize,
    additional_bytes: usize,
}

fn replay_splittable_message_expansion(
    item: &ResponseItem,
    item_bytes: usize,
    budget: &ModelContextBudget,
) -> ThreadStoreResult<Option<ReplayMessageExpansion>> {
    let ResponseItem::Message {
        id,
        role,
        content,
        phase,
        internal_chat_message_metadata_passthrough: metadata,
    } = item
    else {
        return Ok(None);
    };
    if content.is_empty() {
        return Ok(None);
    }

    let message_with_content = |content| ResponseItem::Message {
        id: id.clone(),
        role: role.clone(),
        content,
        phase: phase.clone(),
        internal_chat_message_metadata_passthrough: metadata.clone(),
    };
    let mut expanded_items = 0usize;
    let mut expanded_bytes = 0usize;
    let mut account = |item_count: usize, item_bytes: usize| -> ThreadStoreResult<()> {
        expanded_items = expanded_items
            .checked_add(item_count)
            .ok_or_else(|| budget.limit_error("item count overflow"))?;
        expanded_bytes = expanded_bytes
            .checked_add(item_bytes)
            .ok_or_else(|| budget.limit_error("byte count overflow"))?;
        Ok(())
    };
    let empty_group_bytes = serialized_bytes(&message_with_content(Vec::new()))?;
    let mut current_group_bytes: Option<usize> = None;
    for content in content {
        let single_item = message_with_content(vec![content.clone()]);
        let single_item_bytes = serialized_bytes(&single_item)?;
        if single_item_bytes <= max_model_context_item_bytes()
            || has_discounted_model_payload(&single_item)
        {
            let content_bytes = single_item_bytes.saturating_sub(empty_group_bytes);
            let candidate_bytes = match current_group_bytes {
                Some(group_bytes) => group_bytes
                    .checked_add(content_bytes)
                    .and_then(|bytes| bytes.checked_add(1))
                    .ok_or_else(|| budget.limit_error("byte count overflow"))?,
                None => single_item_bytes,
            };
            if candidate_bytes <= max_model_context_item_bytes()
                || current_group_bytes.is_none() && has_discounted_model_payload(&single_item)
            {
                current_group_bytes = Some(candidate_bytes);
                continue;
            }
            if let Some(group_bytes) = current_group_bytes.replace(single_item_bytes) {
                account(/*item_count*/ 1, group_bytes)?;
            }
            continue;
        }

        if let Some(group_bytes) = current_group_bytes.take() {
            account(/*item_count*/ 1, group_bytes)?;
        }
        let empty_content = match content {
            ContentItem::InputText { .. } => ContentItem::InputText {
                text: String::new(),
            },
            ContentItem::OutputText { .. } => ContentItem::OutputText {
                text: String::new(),
            },
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => return Ok(None),
        };
        let empty_item_bytes = serialized_bytes(&message_with_content(vec![empty_content]))?;
        // Leave enough room for any one JSON-escaped Unicode scalar at a chunk boundary.
        let text_capacity = max_model_context_item_bytes()
            .saturating_sub(empty_item_bytes)
            .saturating_sub(12);
        if text_capacity == 0 {
            return Ok(None);
        }
        let text_bytes = single_item_bytes.saturating_sub(empty_item_bytes);
        let item_count = text_bytes.div_ceil(text_capacity).max(1);
        let split_item_bytes = empty_item_bytes
            .checked_mul(item_count)
            .and_then(|bytes| bytes.checked_add(text_bytes))
            .ok_or_else(|| budget.limit_error("byte count overflow"))?;
        account(item_count, split_item_bytes)?;
    }
    if let Some(group_bytes) = current_group_bytes {
        account(/*item_count*/ 1, group_bytes)?;
    }
    Ok(Some(ReplayMessageExpansion {
        additional_items: expanded_items.saturating_sub(1),
        additional_bytes: expanded_bytes.saturating_sub(item_bytes),
    }))
}

fn has_discounted_model_payload(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { content, .. } => content.iter().any(|content| {
            matches!(
                content,
                ContentItem::InputImage { .. } | ContentItem::InputAudio { .. }
            )
        }),
        ResponseItem::Reasoning {
            encrypted_content: Some(_),
            ..
        }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction {
            encrypted_content: Some(_),
            ..
        } => true,
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.content_items().is_some_and(|items| {
                items.iter().any(|item| {
                    matches!(
                        item,
                        FunctionCallOutputContentItem::InputImage { .. }
                            | FunctionCallOutputContentItem::InputAudio { .. }
                            | FunctionCallOutputContentItem::EncryptedContent { .. }
                    )
                })
            })
        }
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::Other => false,
    }
}

fn replay_truncatable_output_minimum_fits(
    item: &ResponseItem,
    budget: &ModelContextBudget,
) -> ThreadStoreResult<bool> {
    let mut minimum = item.clone();
    let output = match &mut minimum {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output,
        _ => return Ok(false),
    };
    match &mut output.body {
        FunctionCallOutputBody::Text(text) => text.clear(),
        FunctionCallOutputBody::ContentItems(items) => {
            for item in items {
                match item {
                    FunctionCallOutputContentItem::InputText { text } => text.clear(),
                    FunctionCallOutputContentItem::InputImage { .. }
                    | FunctionCallOutputContentItem::InputAudio { .. }
                    | FunctionCallOutputContentItem::EncryptedContent { .. } => {}
                }
            }
        }
    }
    let fits = serialized_bytes(&minimum)? <= max_model_context_item_bytes();
    if !fits {
        return Err(budget.limit_error(&format!(
            "an individual model-visible history item exceeds {MAX_MODEL_CONTEXT_ITEM_TOKENS} estimated tokens and cannot be truncated safely"
        )));
    }
    Ok(true)
}

fn serialized_bytes(value: &(impl serde::Serialize + ?Sized)) -> ThreadStoreResult<usize> {
    serde_json::to_vec(value)
        .map(|serialized| serialized.len())
        .map_err(serialization_error)
}

pub(super) async fn load_latest_model_context(
    store: &PostgresThreadStore,
    params: LoadThreadHistoryParams,
) -> ThreadStoreResult<StoredModelContext> {
    let mut transaction = store
        .pool
        .begin()
        .await
        .map_err(|error| database_error("load latest model context", error))?;
    let projection = sqlx::query(AssertSqlSafe(format!(
        "SELECT projection FROM {} WHERE thread_id = $1 FOR SHARE",
        store.tables.threads
    )))
    .bind(params.thread_id.to_string())
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error("load latest model context", error))?
    .ok_or(ThreadStoreError::ThreadNotFound {
        thread_id: params.thread_id,
    })?;
    let projection: Value = projection
        .try_get("projection")
        .map_err(|error| database_error("load latest model context", error))?;
    let thread: StoredThread = serde_json::from_value(projection).map_err(serialization_error)?;
    if !params.include_archived && thread.archived_at.is_some() {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("thread {} is archived", params.thread_id),
        });
    }

    let head = sqlx::query(AssertSqlSafe(format!(
        "SELECT CASE WHEN octet_length(item::text) <= $2 THEN item END AS item, \
         octet_length(item::text)::bigint AS item_bytes \
         FROM {} WHERE thread_id = $1 AND ordinal = 0",
        store.tables.history
    )))
    .bind(params.thread_id.to_string())
    .bind(i64::try_from(MAX_MODEL_CONTEXT_BYTES).unwrap_or(i64::MAX))
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error("load latest model context", error))?
    .ok_or_else(|| ThreadStoreError::Internal {
        message: format!("thread {} has no session metadata", params.thread_id),
    })?;
    let head_bytes: i64 = head
        .try_get("item_bytes")
        .map_err(|error| database_error("load latest model context", error))?;
    let mut budget = ModelContextBudget::new(params.thread_id);
    budget.account_item(head_bytes)?;
    let head = bounded_item_from_row(&head, &budget)?;
    validate_model_context_item(&head, &mut budget)?;
    let RolloutItem::SessionMeta(session_meta) = head else {
        return Err(ThreadStoreError::Internal {
            message: format!("thread {} has invalid session metadata", params.thread_id),
        });
    };
    if session_meta.meta.id != params.thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "thread history belongs to {}, not {}",
                session_meta.meta.id, params.thread_id
            ),
        });
    }

    let items = if matches!(session_meta.meta.history_mode, ThreadHistoryMode::Paginated) {
        let mut scan = ModelContextScan::default();
        let mut reached_bounded_cutoff = false;
        let mut rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT ordinal, CASE WHEN octet_length(item::text) <= $3 THEN item END AS item, \
             octet_length(item::text)::bigint AS item_bytes \
             FROM {} WHERE thread_id = $1 AND ordinal > 0 ORDER BY ordinal DESC LIMIT $2",
            store.tables.history
        )))
        .bind(params.thread_id.to_string())
        .bind(i64::try_from(MAX_MODEL_CONTEXT_ITEMS).unwrap_or(i64::MAX))
        .bind(i64::try_from(MAX_MODEL_CONTEXT_BYTES).unwrap_or(i64::MAX))
        .fetch(transaction.as_mut());
        while let Some(row) = rows
            .try_next()
            .await
            .map_err(|error| database_error("load latest model context", error))?
        {
            let item_bytes: i64 = row
                .try_get("item_bytes")
                .map_err(|error| database_error("load latest model context", error))?;
            budget.account_item(item_bytes)?;
            let item = bounded_item_from_row(&row, &budget)?;
            if !matches!(&item, RolloutItem::SessionMeta(_)) {
                validate_model_context_item(&item, &mut budget)?;
            }
            if matches!(scan.push(item), ModelContextScanProgress::Complete) {
                reached_bounded_cutoff = true;
                break;
            }
        }
        drop(rows);
        let mut items = scan.finish(session_meta.clone());
        if !reached_bounded_cutoff {
            items.insert(0, RolloutItem::SessionMeta(session_meta));
        }
        items
    } else {
        let mut rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT ordinal, CASE WHEN octet_length(item::text) <= $3 THEN item END AS item, \
             octet_length(item::text)::bigint AS item_bytes \
             FROM {} WHERE thread_id = $1 AND ordinal > 0 ORDER BY ordinal ASC LIMIT $2",
            store.tables.history
        )))
        .bind(params.thread_id.to_string())
        .bind(i64::try_from(MAX_MODEL_CONTEXT_ITEMS).unwrap_or(i64::MAX))
        .bind(i64::try_from(MAX_MODEL_CONTEXT_BYTES).unwrap_or(i64::MAX))
        .fetch(transaction.as_mut());
        let mut items = vec![RolloutItem::SessionMeta(session_meta)];
        while let Some(row) = rows
            .try_next()
            .await
            .map_err(|error| database_error("load latest model context", error))?
        {
            let item_bytes: i64 = row
                .try_get("item_bytes")
                .map_err(|error| database_error("load latest model context", error))?;
            budget.account_item(item_bytes)?;
            let item = bounded_item_from_row(&row, &budget)?;
            if !matches!(&item, RolloutItem::SessionMeta(_)) {
                validate_model_context_item(&item, &mut budget)?;
            }
            items.push(item);
        }
        drop(rows);
        items
    };

    transaction
        .commit()
        .await
        .map_err(|error| database_error("load latest model context", error))?;
    Ok(StoredModelContext {
        thread_id: params.thread_id,
        items,
    })
}

#[cfg(test)]
#[path = "model_context_tests.rs"]
mod tests;
