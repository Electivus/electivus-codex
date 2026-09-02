use crate::context::ContextualUserFragment;
use crate::context::ModelSwitchInstructions;
use crate::context::world_state::WorldState;
use crate::context::world_state::WorldStateSnapshot;
use crate::context_manager::normalize;
use crate::context_manager::replay::MAX_REPLAY_HISTORY_BYTES;
use crate::context_manager::replay::MAX_REPLAY_HISTORY_ITEMS;
use crate::context_manager::replay::process_replayed_annotated_item;
use crate::context_manager::replay::process_replayed_item;
use crate::context_manager::replay::truncate_output_item;
use crate::event_mapping::has_non_contextual_dev_message_content;
use crate::event_mapping::is_contextual_dev_message_content;
use crate::event_mapping::is_contextual_user_message_content;
use crate::session::turn_context::TurnContext;
use codex_extension_api::ConversationHistorySnapshot;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::InputModality;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::WorldStateItem;
use codex_utils_audio::estimate_audio_token_count;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_function_output_items_with_policy;
use codex_utils_output_truncation::truncate_text;
use std::ops::Deref;
use std::sync::Arc;

pub(crate) use codex_model_context::estimate_response_item_token_count as estimate_item_token_count;

/// Transcript of thread history
#[derive(Debug, Clone, Default)]
pub(crate) struct ContextManager {
    /// The oldest items are at the beginning of the vector. Snapshots share the vector until a
    /// caller needs to mutate it, avoiding deep copies for read-only history consumers.
    items: Arc<Vec<ResponseItemEnvelope>>,
    /// Bumped whenever history is rewritten, such as compaction or rollback.
    history_version: u64,
    token_info: Option<TokenUsageInfo>,
    /// Reference context snapshot used for diffing and producing model-visible
    /// settings update items.
    ///
    /// This is the baseline for the next regular model turn, and may already
    /// match the current turn after context updates are persisted.
    ///
    /// When this is `None`, settings diffing treats the next turn as having no
    /// baseline and emits a full reinjection of context state. Rollback may
    /// also clear this when it trims a mixed initial-context developer bundle
    /// whose non-diff fragments no longer exist in the surviving history.
    reference_context_item: Option<TurnContextItem>,
    /// World state most recently appended to model-visible history.
    world_state_baseline: Option<WorldStateSnapshot>,
    /// Number of leading items restored from durable replay. Request construction may trim only
    /// this prefix when later model-visible request components consume the remaining budget.
    replay_prefix_items: usize,
    /// Whether this history was reconstructed from durable storage, including an empty model-
    /// visible replay whose metadata must still be bounded at request construction.
    replayed_history: bool,
}

struct SharedConversationHistory {
    items: Arc<Vec<ResponseItemEnvelope>>,
}

impl ConversationHistorySnapshot for SharedConversationHistory {
    fn items(&self) -> Box<dyn Iterator<Item = &ResponseItem> + Send + '_> {
        Box::new(
            self.items
                .iter()
                .map(|envelope| &envelope.item)
                .filter(|item| {
                    !matches!(
                        item,
                        ResponseItem::Message { role, content, .. }
                            if role == "user" && is_contextual_user_message_content(content)
                    )
                }),
        )
    }
}

impl ContextManager {
    pub(crate) fn new() -> Self {
        Self {
            items: Arc::new(Vec::new()),
            history_version: 0,
            token_info: TokenUsageInfo::new_or_append(
                &None, &None, /*model_context_window*/ None,
            ),
            reference_context_item: None,
            world_state_baseline: None,
            replay_prefix_items: 0,
            replayed_history: false,
        }
    }

    pub(crate) fn conversation_history_snapshot(&self) -> Arc<dyn ConversationHistorySnapshot> {
        Arc::new(SharedConversationHistory {
            items: Arc::clone(&self.items),
        })
    }

    pub(crate) fn token_info(&self) -> Option<TokenUsageInfo> {
        self.token_info.clone()
    }

    pub(crate) fn set_token_info(&mut self, info: Option<TokenUsageInfo>) {
        self.token_info = info;
    }

    pub(crate) fn set_reference_context_item(&mut self, item: Option<TurnContextItem>) {
        self.reference_context_item = item;
    }

    pub(crate) fn reference_context_item(&self) -> Option<TurnContextItem> {
        self.reference_context_item.clone()
    }

    pub(crate) fn update_world_state(
        &mut self,
        world_state: &WorldState,
    ) -> (Vec<Box<dyn ContextualUserFragment>>, Option<WorldStateItem>) {
        let snapshot = world_state.snapshot();
        let fragments =
            world_state.render_history_diff(self.world_state_baseline.as_ref(), self.raw_items());
        let rollout_item = self.world_state_baseline.as_ref().map_or_else(
            || Some(WorldStateItem::full(snapshot.clone().into_object())),
            |previous| {
                snapshot
                    .merge_patch_from(previous)
                    .map(WorldStateItem::patch)
            },
        );
        self.world_state_baseline = Some(snapshot);
        (fragments, rollout_item)
    }

    pub(crate) fn set_world_state_baseline(&mut self, snapshot: WorldStateSnapshot) {
        self.world_state_baseline = Some(snapshot);
    }

    pub(crate) fn set_token_usage_full(&mut self, context_window: i64) {
        match &mut self.token_info {
            Some(info) => info.fill_to_context_window(context_window),
            None => {
                self.token_info = Some(TokenUsageInfo::full_context_window(context_window));
            }
        }
    }

    /// `items` is ordered from oldest to newest.
    pub(crate) fn record_items<I>(&mut self, items: I, policy: TruncationPolicy)
    where
        I: IntoIterator,
        I::Item: Deref<Target = ResponseItem>,
    {
        self.record_items_with_metadata(items.into_iter().map(|item| (item, None)), policy);
    }

    /// Records history envelopes while preserving their history-only metadata.
    pub(crate) fn record_annotated_items(
        &mut self,
        items: &[ResponseItemEnvelope],
        policy: TruncationPolicy,
    ) {
        self.record_items_with_metadata(
            items
                .iter()
                .map(|envelope| (&envelope.item, envelope.metadata.as_ref())),
            policy,
        );
    }

    fn record_items_with_metadata<'a, I, T>(&mut self, items: I, policy: TruncationPolicy)
    where
        I: IntoIterator<Item = (T, Option<&'a CodexHarnessMetadata>)>,
        T: Deref<Target = ResponseItem>,
    {
        for (item, metadata) in items {
            let item = item.deref();
            if !is_api_message(item) {
                continue;
            }

            let processed = ResponseItemEnvelope {
                item: Self::process_item(item, policy),
                metadata: metadata.cloned(),
            };
            Arc::make_mut(&mut self.items).push(processed);
        }
    }

    /// Returns the history prepared for sending to the model. This applies a proper
    /// normalization and drops un-suited items. Unsupported image and audio content
    /// is stripped from messages and tool outputs according to `input_modalities`.
    pub(crate) fn for_prompt(self, input_modalities: &[InputModality]) -> Vec<ResponseItem> {
        self.for_prompt_annotated(input_modalities)
            .into_iter()
            .map(ResponseItemEnvelope::into_item)
            .collect()
    }

    /// Returns normalized history envelopes for internal consumers that must retain metadata.
    pub(crate) fn for_prompt_annotated(
        mut self,
        input_modalities: &[InputModality],
    ) -> Vec<ResponseItemEnvelope> {
        let contains_only_replay =
            self.replayed_history && self.replay_prefix_items >= self.items.len();
        self.normalize_history(input_modalities);
        if contains_only_replay {
            enforce_replay_limits(Arc::make_mut(&mut self.items));
        }
        Arc::unwrap_or_clone(self.items)
    }

    pub(crate) fn replay_prefix_items(&self) -> usize {
        self.replay_prefix_items.min(self.items.len())
    }

    pub(crate) fn is_replayed_history(&self) -> bool {
        self.replayed_history
    }

    /// Iterates over raw response items without exposing their history envelopes.
    pub(crate) fn raw_items(
        &self,
    ) -> impl Clone + ExactSizeIterator<Item = &ResponseItem> + DoubleEndedIterator {
        self.items.iter().map(|envelope| &envelope.item)
    }

    /// Returns annotated history items without cloning their response payloads.
    pub(crate) fn annotated_items(&self) -> &[ResponseItemEnvelope] {
        &self.items
    }

    /// Returns raw items in the history and consumes the snapshot.
    pub(crate) fn into_raw_items(self) -> Vec<ResponseItem> {
        self.into_annotated_items()
            .into_iter()
            .map(ResponseItemEnvelope::into_item)
            .collect()
    }

    /// Returns annotated history items and consumes the snapshot.
    pub(crate) fn into_annotated_items(self) -> Vec<ResponseItemEnvelope> {
        Arc::unwrap_or_clone(self.items)
    }

    pub(crate) fn history_version(&self) -> u64 {
        self.history_version
    }

    // Estimate token usage using byte-based heuristics from the truncation helpers.
    // This is a coarse lower bound, not a tokenizer-accurate count.
    pub(crate) fn estimate_token_count(&self, turn_context: &TurnContext) -> Option<i64> {
        let model_info = &turn_context.model_info;
        let personality = turn_context.personality.or(turn_context.config.personality);
        let base_instructions = BaseInstructions {
            text: model_info.get_model_instructions(personality),
            provenance: None,
        };
        self.estimate_token_count_with_base_instructions(&base_instructions)
    }

    pub(crate) fn estimate_token_count_with_base_instructions(
        &self,
        base_instructions: &BaseInstructions,
    ) -> Option<i64> {
        let base_tokens =
            i64::try_from(approx_token_count(&base_instructions.text)).unwrap_or(i64::MAX);

        let items_tokens = self
            .items
            .iter()
            .map(|envelope| estimate_item_token_count(&envelope.item))
            .fold(0i64, i64::saturating_add);

        Some(base_tokens.saturating_add(items_tokens))
    }

    pub(crate) fn remove_first_item(&mut self) {
        if !self.items.is_empty() {
            // Remove the oldest item (front of the list). Items are ordered from
            // oldest → newest, so index 0 is the first entry recorded.
            let items = Arc::make_mut(&mut self.items);
            let removed = items.remove(0);
            // If the removed item participates in a call/output pair, also remove
            // its corresponding counterpart to keep the invariants intact without
            // running a full normalization pass.
            let removed_items = 1 + usize::from(
                normalize::remove_corresponding_for(items, &removed.item).is_some(),
            );
            self.replay_prefix_items = self.replay_prefix_items.saturating_sub(removed_items);
            self.world_state_baseline = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn replace(&mut self, items: Vec<ResponseItem>) {
        self.replace_annotated(items.into_iter().map(ResponseItemEnvelope::new).collect());
    }

    pub(crate) fn replace_annotated(&mut self, items: Vec<ResponseItemEnvelope>) {
        self.items = Arc::new(items);
        self.history_version = self.history_version.saturating_add(1);
        self.world_state_baseline = None;
        self.replay_prefix_items = 0;
        self.replayed_history = false;
    }

    #[cfg(test)]
    pub(crate) fn replace_replayed(&mut self, items: Vec<ResponseItem>) {
        self.replace(items);
        self.replay_prefix_items = self.items.len();
        self.replayed_history = true;
    }

    pub(crate) fn replace_annotated_replayed(&mut self, items: Vec<ResponseItemEnvelope>) {
        self.replace_annotated(items);
        self.replay_prefix_items = self.items.len();
        self.replayed_history = true;
    }

    #[cfg(test)]
    pub(crate) fn replace_with_policy(&mut self, items: &[ResponseItem], policy: TruncationPolicy) {
        let start = items.len().saturating_sub(MAX_REPLAY_HISTORY_ITEMS);
        let processed = items
            .get(start..)
            .unwrap_or_default()
            .iter()
            .filter(|item| is_api_message(item))
            .filter_map(|item| process_replayed_item(item, policy))
            .collect();
        self.replace(processed);
        self.replay_prefix_items = self.items.len();
        self.replayed_history = true;
    }

    pub(crate) fn replace_annotated_with_policy(
        &mut self,
        items: &[ResponseItemEnvelope],
        policy: TruncationPolicy,
    ) {
        let start = items.len().saturating_sub(MAX_REPLAY_HISTORY_ITEMS);
        let processed = items
            .get(start..)
            .unwrap_or_default()
            .iter()
            .filter(|envelope| is_api_message(&envelope.item))
            .filter_map(|envelope| {
                process_replayed_annotated_item(&envelope.item, envelope.metadata.as_ref(), policy)
                    .map(|item| ResponseItemEnvelope {
                        item,
                        metadata: envelope.metadata.clone(),
                    })
            })
            .collect();
        self.replace_annotated(processed);
        self.replay_prefix_items = self.items.len();
        self.replayed_history = true;
    }

    pub(crate) fn record_replayed_items<'a>(
        &mut self,
        items: impl IntoIterator<Item = &'a ResponseItem>,
        policy: TruncationPolicy,
    ) {
        let remaining = MAX_REPLAY_HISTORY_ITEMS.saturating_sub(self.items.len());
        let processed_items = items
            .into_iter()
            .filter(|item| is_api_message(item))
            .filter_map(|item| process_replayed_item(item, policy).map(ResponseItemEnvelope::new))
            .take(remaining);
        Arc::make_mut(&mut self.items).extend(processed_items);
        self.replay_prefix_items = self.items.len();
        self.replayed_history = true;
    }

    pub(crate) fn record_replayed_annotated_items(
        &mut self,
        items: &[ResponseItemEnvelope],
        policy: TruncationPolicy,
    ) {
        let remaining = MAX_REPLAY_HISTORY_ITEMS.saturating_sub(self.items.len());
        let processed_items = items
            .iter()
            .filter(|envelope| is_api_message(&envelope.item))
            .filter_map(|envelope| {
                process_replayed_annotated_item(&envelope.item, envelope.metadata.as_ref(), policy)
                    .map(|item| ResponseItemEnvelope {
                        item,
                        metadata: envelope.metadata.clone(),
                    })
            })
            .take(remaining);
        Arc::make_mut(&mut self.items).extend(processed_items);
        self.replay_prefix_items = self.items.len();
        self.replayed_history = true;
    }

    /// Drop the last `num_turns` instruction turns from this history.
    ///
    /// Instruction turns are history messages that should behave like a new prompt boundary:
    /// ordinary user messages and structured assistant inter-agent instructions.
    ///
    /// This mirrors thread-rollback semantics:
    /// - `num_turns == 0` is a no-op
    /// - if there are no user turns, this is a no-op
    /// - if `num_turns` exceeds the number of user turns, all user turns are dropped while
    ///   preserving any items that occurred before the first user message.
    ///
    /// If rollback trims a pre-turn developer message that mixes contextual fragments with
    /// persistent developer text from `build_initial_context`, this also clears
    /// `reference_context_item`. The surviving history no longer contains the full bundle that
    /// established the prior baseline, so future turns must fall back to full reinjection instead
    /// of diffing against stale state.
    pub(crate) fn drop_last_n_user_turns(&mut self, num_turns: u32) {
        if num_turns == 0 {
            return;
        }

        let snapshot = self.items.clone();
        let replay_prefix_items = self.replay_prefix_items;
        let replayed_history = self.replayed_history;
        let user_positions = user_message_positions(&snapshot);
        let Some(&first_instruction_turn_idx) = user_positions.first() else {
            self.replace_annotated(Arc::unwrap_or_clone(snapshot));
            self.replay_prefix_items = replay_prefix_items.min(self.items.len());
            self.replayed_history = replayed_history;
            return;
        };

        let n_from_end = usize::try_from(num_turns).unwrap_or(usize::MAX);
        let mut cut_idx = if n_from_end >= user_positions.len() {
            first_instruction_turn_idx
        } else {
            user_positions[user_positions.len() - n_from_end]
        };

        cut_idx =
            self.trim_pre_turn_context_updates(&snapshot, first_instruction_turn_idx, cut_idx);

        let mut retained_items = snapshot[..cut_idx].to_vec();
        if cut_idx == first_instruction_turn_idx
            && let Some(first_turn_id) = snapshot[first_instruction_turn_idx].turn_id()
        {
            retained_items.retain_mut(|item| {
                if item.turn_id() == Some(first_turn_id)
                    && let ResponseItem::Message { role, content, .. } = &mut item.item
                    && role == "developer"
                {
                    content.retain(|content| {
                        !matches!(
                            content,
                            ContentItem::InputText { text }
                                if ModelSwitchInstructions::matches_text(text)
                        )
                    });
                    !content.is_empty()
                } else {
                    true
                }
            });
        }

        let retained_replay_items = replay_prefix_items.min(retained_items.len());
        self.replace_annotated(retained_items);
        self.replay_prefix_items = retained_replay_items;
        self.replayed_history = replayed_history;
    }

    pub(crate) fn update_token_info(
        &mut self,
        usage: &TokenUsage,
        model_context_window: Option<i64>,
    ) {
        self.token_info = TokenUsageInfo::new_or_append(
            &self.token_info,
            &Some(usage.clone()),
            model_context_window,
        );
    }

    fn get_non_last_reasoning_items_tokens(&self) -> i64 {
        // Get reasoning items excluding all the ones after the last instruction boundary.
        let Some(last_user_index) = self
            .items
            .iter()
            .rposition(|envelope| is_user_turn_boundary(&envelope.item))
        else {
            return 0;
        };

        self.items
            .iter()
            .take(last_user_index)
            .filter(|envelope| {
                matches!(
                    &envelope.item,
                    ResponseItem::Reasoning {
                        encrypted_content: Some(_),
                        ..
                    }
                )
            })
            .map(|envelope| estimate_item_token_count(&envelope.item))
            .fold(0i64, i64::saturating_add)
    }

    // These are local items added after the most recent model-emitted item.
    // They are not reflected in `last_token_usage.total_tokens`.
    fn items_after_last_model_generated_item(
        &self,
    ) -> impl Clone + ExactSizeIterator<Item = &ResponseItem> + DoubleEndedIterator {
        let start = self
            .items
            .iter()
            .rposition(|envelope| is_model_generated_item(&envelope.item))
            .map_or(self.items.len(), |index| index.saturating_add(1));
        self.items[start..].iter().map(|envelope| &envelope.item)
    }

    /// When true, the server already accounted for past reasoning tokens and
    /// the client should not re-estimate them.
    pub(crate) fn get_total_token_usage(&self, server_reasoning_included: bool) -> i64 {
        let last_tokens = self
            .token_info
            .as_ref()
            .map(|info| info.last_token_usage.total_tokens)
            .unwrap_or(0);
        let items_after_last_model_generated_tokens = self
            .items_after_last_model_generated_item()
            .map(estimate_item_token_count)
            .fold(0i64, i64::saturating_add);
        if server_reasoning_included {
            last_tokens.saturating_add(items_after_last_model_generated_tokens)
        } else {
            last_tokens
                .saturating_add(self.get_non_last_reasoning_items_tokens())
                .saturating_add(items_after_last_model_generated_tokens)
        }
    }

    pub(crate) fn estimated_tokens_after_last_model_generated_item(&self) -> i64 {
        self.items_after_last_model_generated_item()
            .map(estimate_item_token_count)
            .fold(0i64, i64::saturating_add)
    }

    /// This function enforces a couple of invariants on the in-memory history:
    /// 1. every call (function/custom) has a corresponding output entry
    /// 2. every output has a corresponding call entry or names an external tool event
    /// 3. unsupported image and audio content is stripped from messages and tool outputs
    fn normalize_history(&mut self, input_modalities: &[InputModality]) {
        let items = Arc::make_mut(&mut self.items);

        // all function/tool calls must have a corresponding output
        normalize::ensure_call_outputs_present(items);

        // Paired outputs must have a corresponding call; named external outputs stand alone.
        normalize::remove_orphan_outputs(items);

        // strip images when model does not support them
        normalize::strip_images_when_unsupported(input_modalities, items);

        // strip audio when model does not support it
        normalize::strip_audio_when_unsupported(input_modalities, items);
    }

    #[cfg(test)]
    pub(crate) fn prepare_replayed_history(self) -> Vec<ResponseItem> {
        self.prepare_replayed_annotated_history()
            .into_iter()
            .map(ResponseItemEnvelope::into_item)
            .collect()
    }

    pub(crate) fn prepare_replayed_annotated_history(mut self) -> Vec<ResponseItemEnvelope> {
        let items = Arc::make_mut(&mut self.items);
        normalize::ensure_call_outputs_present(items);
        normalize::remove_orphan_outputs(items);
        enforce_replay_limits(items);
        Arc::unwrap_or_clone(self.items)
    }

    fn process_item(item: &ResponseItem, policy: TruncationPolicy) -> ResponseItem {
        let policy_with_serialization_budget = policy * 1.2;
        match item {
            ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. } => {
                truncate_output_item(item, policy_with_serialization_budget)
            }
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::Message { .. }
            | ResponseItem::AgentMessage { .. }
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
            | ResponseItem::Other => item.clone(),
        }
    }

    /// Walk backward from a rollback cut and trim contiguous pre-turn context-update items.
    ///
    /// Returns the adjusted cut index after removing contextual developer/user items immediately
    /// above the rolled-back turn boundary.
    ///
    /// `first_instruction_turn_idx` is the earliest rollback-eligible instruction-turn boundary
    /// in `snapshot`; the trim walk never crosses it so any session-prefix items that predate the
    /// first real turn survive rollback.
    ///
    /// `cut_idx` is the tentative slice boundary after dropping the requested number of
    /// instruction turns, before stripping contextual pre-turn items that sit immediately above
    /// that boundary.
    ///
    /// If any trimmed developer message was a mixed `build_initial_context` bundle containing both
    /// rollback-trimmable contextual fragments and persistent developer text, this also clears the
    /// stored `reference_context_item` baseline so the next real turn falls back to full
    /// reinjection.
    fn trim_pre_turn_context_updates(
        &mut self,
        snapshot: &[ResponseItemEnvelope],
        first_instruction_turn_idx: usize,
        mut cut_idx: usize,
    ) -> usize {
        while cut_idx > first_instruction_turn_idx {
            match &snapshot[cut_idx - 1].item {
                ResponseItem::Message { role, content, .. }
                    if role == "developer" && is_contextual_dev_message_content(content) =>
                {
                    if has_non_contextual_dev_message_content(content) {
                        // Mixed `build_initial_context` bundles are not reconstructible from
                        // steady-state diffs once trimmed, so the next real turn must fully
                        // reinject context instead of diffing against a stale baseline.
                        self.reference_context_item = None;
                    }
                    cut_idx -= 1;
                }
                ResponseItem::Message { role, content, .. }
                    if role == "user" && is_contextual_user_message_content(content) =>
                {
                    cut_idx -= 1;
                }
                _ => break,
            }
        }
        cut_idx
    }
}

fn enforce_replay_limits(items: &mut Vec<ResponseItemEnvelope>) {
    while let Some(index) = items.iter().position(|envelope| {
        estimate_item_token_count(&envelope.item) > 10_000
            && !matches!(
                envelope.item,
                ResponseItem::Message { .. }
                    | ResponseItem::AgentMessage { .. }
                    | ResponseItem::Reasoning { .. }
                    | ResponseItem::Compaction { .. }
                    | ResponseItem::ContextCompaction { .. }
            )
    }) {
        let removed = items.remove(index);
        normalize::remove_corresponding_for(items, &removed.item);
    }

    let mut serialized_bytes = items.iter().fold(0u64, |total, item| {
        total.saturating_add(serialized_response_item_bytes_u64(&item.item))
    });
    while items.len() > MAX_REPLAY_HISTORY_ITEMS || serialized_bytes > MAX_REPLAY_HISTORY_BYTES {
        let removed = items.remove(0);
        serialized_bytes =
            serialized_bytes.saturating_sub(serialized_response_item_bytes_u64(&removed.item));
        if let Some(corresponding) = normalize::remove_corresponding_for(items, &removed.item) {
            serialized_bytes = serialized_bytes
                .saturating_sub(serialized_response_item_bytes_u64(&corresponding.item));
        }
    }
}

fn serialized_response_item_bytes_u64(item: &ResponseItem) -> u64 {
    u64::try_from(serialized_response_item_bytes(item)).unwrap_or(u64::MAX)
}

fn serialized_response_item_bytes(item: &ResponseItem) -> i64 {
    serde_json::to_string(item)
        .map(|serialized| i64::try_from(serialized.len()).unwrap_or(i64::MAX))
        .unwrap_or(i64::MAX)
}

pub(crate) fn truncate_function_output_payload(
    output: &FunctionCallOutputPayload,
    policy: TruncationPolicy,
) -> FunctionCallOutputPayload {
    let body = match &output.body {
        FunctionCallOutputBody::Text(content) => {
            FunctionCallOutputBody::Text(truncate_text(content, policy))
        }
        FunctionCallOutputBody::ContentItems(items) => FunctionCallOutputBody::ContentItems(
            truncate_function_output_items_with_policy(items, policy, estimate_audio_token_count),
        ),
    };

    FunctionCallOutputPayload {
        body,
        success: output.success,
    }
}

/// API messages include every non-system item (user/assistant messages, reasoning,
/// tool calls, tool outputs, shell calls, web-search calls, and image-generation
/// calls).
fn is_api_message(message: &ResponseItem) -> bool {
    match message {
        ResponseItem::Message { role, .. } => role.as_str() != "system",
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. } => true,
        ResponseItem::CompactionTrigger { .. } => false,
        ResponseItem::Other => false,
    }
}

fn is_model_generated_item(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, .. } => role == "assistant",
        ResponseItem::Reasoning { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. } => true,
        ResponseItem::CompactionTrigger { .. } => false,
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::Other => false,
    }
}

pub(crate) fn is_user_turn_boundary(item: &ResponseItem) -> bool {
    if matches!(item, ResponseItem::AgentMessage { .. }) {
        return true;
    }
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };

    (role == "user" && !is_contextual_user_message_content(content))
        || (role == "assistant" && is_inter_agent_instruction_content(content))
}

fn is_inter_agent_instruction_content(content: &[ContentItem]) -> bool {
    InterAgentCommunication::is_message_content(content)
}

fn user_message_positions(items: &[ResponseItemEnvelope]) -> Vec<usize> {
    let mut positions = Vec::new();
    for (idx, envelope) in items.iter().enumerate() {
        if is_user_turn_boundary(&envelope.item) {
            positions.push(idx);
        }
    }
    positions
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
