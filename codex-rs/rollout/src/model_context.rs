use crate::ResponseItemEnvelope;
use crate::RolloutItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionMetaLine;

/// Whether a reverse model-context scan needs more rollout items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelContextScanProgress {
    /// The reader should provide the next older rollout item.
    Continue,
    /// The scan has collected a safe bounded suffix.
    Complete,
}

/// Minimal persisted event metadata needed while selecting a bounded model-context suffix.
///
/// Storage backends may use these signals instead of loading presentation-only event payloads.
/// Signals affect cutoff selection but are not added to the reconstructed rollout items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelContextScanSignal {
    Compacted {
        has_replacement_history: bool,
        has_window_number: bool,
    },
    ThreadRolledBack,
    ItemCompleted {
        turn_id: String,
        is_user_message: bool,
    },
    TurnComplete {
        turn_id: String,
    },
    TurnAborted {
        turn_id: Option<String>,
    },
    TurnStarted {
        turn_id: String,
    },
    TurnContext {
        turn_id: Option<String>,
    },
    ResponseItem {
        counts_as_user_turn: bool,
    },
    InterAgentCommunication,
    UserMessage,
}

/// Accumulates newest-to-oldest rollout items until they are sufficient to reconstruct the latest
/// model context.
///
/// Storage implementations own how they fetch older items. Local JSONL readers and future
/// reverse-paged cloud readers can both feed their items through this scan to share the cutoff
/// rules and chronological replay assembly.
///
/// The scan stops once it has both:
///
/// - `saw_compaction`: a `CompactedItem` with `replacement_history` and `window_number`;
/// - `saw_completed_turn_context`: a completed user turn with a compatible `TurnContextItem`.
///
/// If the scan reaches the beginning before finding a bounded cutoff, it has already collected
/// the complete replay and so we can return that directly.
///
/// `TurnContextItem` does not identify whether it came from a user turn, so one only counts after
/// the same turn also proves a user-turn boundary: a paginated
/// `ItemCompleted(UserMessage)` marker, agent message, or inter-agent message. Paginated writers
/// persist that marker for real user turns; older rollouts without it conservatively scan to the
/// beginning. A raw `role=user` response item is not sufficient because contextual user fragments
/// use that role but do not count as turn boundaries during reconstruction. The compaction restores
/// model-visible items; the turn context restores previous settings (`model`, `comp_hash`, and
/// `realtime_active`) and the reference baseline.
///
/// These paginated shapes disable the bounded cutoff:
///
/// - compaction without `replacement_history` or `window_number`;
/// - rollback markers;
///
/// When one appears, the scanner continues to the beginning and returns the complete replay.
#[derive(Debug, Default)]
pub struct ModelContextScan {
    items_newest_first: Vec<RolloutItem>,
    saw_compaction: bool,
    saw_completed_turn_context: bool,
    must_scan_to_start: bool,
    active_segment: ActiveTurnSegment,
}

impl ModelContextScan {
    /// Adds the next newest-to-oldest rollout item and reports whether the reader can stop.
    pub fn push(&mut self, item: RolloutItem) -> ModelContextScanProgress {
        let progress = self.observe(&item);
        self.items_newest_first.push(item);
        progress
    }

    /// Observes scan metadata without retaining its presentation-only payload for replay.
    pub fn push_signal(&mut self, signal: ModelContextScanSignal) -> ModelContextScanProgress {
        if self.must_scan_to_start {
            return ModelContextScanProgress::Continue;
        }

        match signal {
            ModelContextScanSignal::Compacted {
                has_replacement_history: true,
                has_window_number: true,
            } => self.saw_compaction = true,
            ModelContextScanSignal::Compacted { .. } | ModelContextScanSignal::ThreadRolledBack => {
                self.must_scan_to_start = true
            }
            ModelContextScanSignal::ItemCompleted {
                turn_id,
                is_user_message,
            } => {
                if self.active_segment.turn_id.is_none() {
                    self.active_segment.turn_id = Some(turn_id.clone());
                }
                if turn_ids_are_compatible(
                    self.active_segment.turn_id.as_deref(),
                    Some(turn_id.as_str()),
                ) {
                    self.active_segment.has_user_turn |= is_user_message;
                }
            }
            ModelContextScanSignal::TurnComplete { turn_id }
            | ModelContextScanSignal::TurnAborted {
                turn_id: Some(turn_id),
            } => {
                self.active_segment.turn_id.get_or_insert(turn_id);
            }
            ModelContextScanSignal::TurnAborted { turn_id: None } => {}
            ModelContextScanSignal::TurnStarted { turn_id } => {
                if turn_ids_are_compatible(
                    self.active_segment.turn_id.as_deref(),
                    Some(turn_id.as_str()),
                ) {
                    self.finalize_active_segment();
                }
            }
            ModelContextScanSignal::TurnContext { turn_id } => {
                if self.active_segment.turn_id.is_none() {
                    self.active_segment.turn_id = turn_id.clone();
                }
                if turn_ids_are_compatible(
                    self.active_segment.turn_id.as_deref(),
                    turn_id.as_deref(),
                ) {
                    self.active_segment.has_turn_context = true;
                }
            }
            ModelContextScanSignal::ResponseItem {
                counts_as_user_turn,
            } => self.active_segment.has_user_turn |= counts_as_user_turn,
            ModelContextScanSignal::InterAgentCommunication
            | ModelContextScanSignal::UserMessage => self.active_segment.has_user_turn = true,
        }

        self.progress()
    }

    /// Returns the collected items in chronological order with canonical head metadata.
    ///
    /// Call this after the reader reaches the beginning of its source or after [`Self::push`]
    /// returns [`ModelContextScanProgress::Complete`].
    pub fn finish(mut self, session_meta: SessionMetaLine) -> Vec<RolloutItem> {
        self.items_newest_first.reverse();
        if self.has_bounded_cutoff() {
            // A bounded scan stops before reaching the head. Prepend the separately loaded head
            // SessionMeta, which remains canonical when copied fork history contains later
            // metadata.
            self.items_newest_first
                .insert(0, RolloutItem::SessionMeta(session_meta));
        }
        self.items_newest_first
    }

    fn observe(&mut self, item: &RolloutItem) -> ModelContextScanProgress {
        if self.must_scan_to_start {
            return ModelContextScanProgress::Continue;
        }

        match item {
            RolloutItem::Compacted(compacted) => {
                return self.push_signal(ModelContextScanSignal::Compacted {
                    has_replacement_history: compacted.replacement_history.is_some(),
                    has_window_number: compacted.window_number.is_some(),
                });
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_)) => {
                return self.push_signal(ModelContextScanSignal::ThreadRolledBack);
            }
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => {
                return self.push_signal(ModelContextScanSignal::ItemCompleted {
                    turn_id: event.turn_id.clone(),
                    is_user_message: matches!(&event.item, TurnItem::UserMessage(_)),
                });
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                return self.push_signal(ModelContextScanSignal::TurnComplete {
                    turn_id: event.turn_id.clone(),
                });
            }
            RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                return self.push_signal(ModelContextScanSignal::TurnAborted {
                    turn_id: event.turn_id.clone(),
                });
            }
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                return self.push_signal(ModelContextScanSignal::TurnStarted {
                    turn_id: event.turn_id.clone(),
                });
            }
            RolloutItem::TurnContext(context) => {
                return self.push_signal(ModelContextScanSignal::TurnContext {
                    turn_id: context.turn_id.clone(),
                });
            }
            RolloutItem::ResponseItem(response_item) => {
                return self.push_signal(ModelContextScanSignal::ResponseItem {
                    counts_as_user_turn: response_item_counts_as_user_turn(response_item),
                });
            }
            RolloutItem::InterAgentCommunication(_) => {
                return self.push_signal(ModelContextScanSignal::InterAgentCommunication);
            }
            RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                return self.push_signal(ModelContextScanSignal::UserMessage);
            }
            RolloutItem::EventMsg(_)
            | RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::SecurityRiskScore(_)
            | RolloutItem::WorldState(_) => {}
        }

        self.progress()
    }

    fn progress(&self) -> ModelContextScanProgress {
        if self.has_bounded_cutoff() {
            ModelContextScanProgress::Complete
        } else {
            ModelContextScanProgress::Continue
        }
    }

    fn finalize_active_segment(&mut self) {
        if self.active_segment.has_user_turn && self.active_segment.has_turn_context {
            self.saw_completed_turn_context = true;
        }
        self.active_segment = ActiveTurnSegment::default();
    }

    fn has_bounded_cutoff(&self) -> bool {
        !self.must_scan_to_start && self.saw_compaction && self.saw_completed_turn_context
    }
}

#[derive(Debug, Default)]
struct ActiveTurnSegment {
    turn_id: Option<String>,
    has_user_turn: bool,
    has_turn_context: bool,
}

fn turn_ids_are_compatible(active_turn_id: Option<&str>, item_turn_id: Option<&str>) -> bool {
    active_turn_id
        .is_none_or(|turn_id| item_turn_id.is_none_or(|item_turn_id| item_turn_id == turn_id))
}

fn response_item_counts_as_user_turn(response_item: &ResponseItemEnvelope) -> bool {
    match &response_item.item {
        ResponseItem::AgentMessage { .. } => true,
        ResponseItem::Message { role, content, .. } => {
            role == "assistant" && InterAgentCommunication::is_message_content(content)
        }
        _ => false,
    }
}
