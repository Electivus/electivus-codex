use crate::context::ContextualUserFragment;
use codex_context_fragments::RenderedFragment;
use codex_model_context::estimate_response_item_model_visible_bytes;
use codex_protocol::ResponseItemId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::TruncationPolicy;

use super::history::estimate_item_token_count;

pub(super) const MAX_MODEL_CONTEXT_ITEM_TOKENS: i64 = 10_000;

fn max_model_context_item_bytes() -> usize {
    TruncationPolicy::Tokens(usize::try_from(MAX_MODEL_CONTEXT_ITEM_TOKENS).unwrap_or(usize::MAX))
        .byte_budget()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MessageGroup {
    Standalone,
    Mergeable,
}

pub(crate) fn build_rendered_message(fragments: Vec<RenderedFragment>) -> Option<ResponseItem> {
    let role = fragments.first()?.role();
    debug_assert!(fragments.iter().all(|fragment| fragment.role() == role));
    let (content, content_item_kinds): (Vec<_>, Vec<_>) = fragments
        .into_iter()
        .map(|fragment| fragment.into_parts().1.into_parts())
        .unzip();

    Some(ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            content_item_kinds: Some(content_item_kinds),
            ..Default::default()
        }),
    })
}

#[derive(Clone, Copy)]
enum TextContentKind {
    Input,
    Output,
}

impl TextContentKind {
    fn content(self, text: String) -> ContentItem {
        match self {
            Self::Input => ContentItem::InputText { text },
            Self::Output => ContentItem::OutputText { text },
        }
    }
}

#[derive(Clone)]
struct MessageContent {
    content: ContentItem,
    kind: Option<ContentItemKind>,
}

struct MessageTemplate {
    id: Option<ResponseItemId>,
    role: String,
    phase: Option<MessagePhase>,
    metadata: Option<InternalChatMessageMetadataPassthrough>,
}

struct AgentMessageTemplate {
    id: Option<ResponseItemId>,
    author: String,
    recipient: String,
    metadata: Option<InternalChatMessageMetadataPassthrough>,
}

impl AgentMessageTemplate {
    fn item(&self, content: Vec<AgentMessageInputContent>) -> ResponseItem {
        ResponseItem::AgentMessage {
            id: self.id.clone(),
            author: self.author.clone(),
            recipient: self.recipient.clone(),
            content,
            internal_chat_message_metadata_passthrough: self.metadata.clone(),
        }
    }

    fn into_items(self, content_groups: Vec<Vec<AgentMessageInputContent>>) -> Vec<ResponseItem> {
        let mut id = self.id.clone();
        content_groups
            .into_iter()
            .map(|content| {
                let mut item = self.item(content);
                item.set_id(id.take());
                item
            })
            .collect()
    }
}

impl MessageTemplate {
    fn item(&self, content: Vec<ContentItem>) -> ResponseItem {
        ResponseItem::Message {
            id: self.id.clone(),
            role: self.role.clone(),
            content,
            phase: self.phase.clone(),
            internal_chat_message_metadata_passthrough: self.metadata.clone(),
        }
    }

    fn into_items(self, content_groups: Vec<Vec<MessageContent>>) -> Vec<ResponseItem> {
        let preserve_content_item_kinds = self
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.content_item_kinds.as_ref())
            .is_some();
        let mut id = self.id.clone();
        content_groups
            .into_iter()
            .map(|content_group| {
                let (content, content_item_kinds): (Vec<_>, Vec<_>) = content_group
                    .into_iter()
                    .map(|content| (content.content, content.kind))
                    .unzip();
                let mut item = self.item(content);
                if preserve_content_item_kinds
                    && let ResponseItem::Message {
                        internal_chat_message_metadata_passthrough: metadata,
                        ..
                    } = &mut item
                {
                    metadata.get_or_insert_default().content_item_kinds = Some(
                        content_item_kinds
                            .into_iter()
                            .map(|kind| {
                                kind.unwrap_or_else(|| ContentItemKind("unknown".to_string()))
                            })
                            .collect(),
                    );
                }
                item.set_id(id.take());
                item
            })
            .collect()
    }
}

/// Losslessly projects splittable messages while preserving their ordered UTF-8 text.
pub(crate) fn split_model_context_item_to_limit(item: ResponseItem) -> Vec<ResponseItem> {
    if estimate_item_token_count(&item) <= MAX_MODEL_CONTEXT_ITEM_TOKENS {
        return vec![item];
    }

    match item {
        ResponseItem::Message {
            id,
            role,
            content,
            phase,
            internal_chat_message_metadata_passthrough: metadata,
        } => split_message(
            MessageTemplate {
                id,
                role,
                phase,
                metadata,
            },
            content,
        ),
        ResponseItem::AgentMessage {
            id,
            author,
            recipient,
            content,
            internal_chat_message_metadata_passthrough: metadata,
        } => split_agent_message(
            AgentMessageTemplate {
                id,
                author,
                recipient,
                metadata,
            },
            content,
        ),
        item => vec![item],
    }
}

fn split_message(template: MessageTemplate, content: Vec<ContentItem>) -> Vec<ResponseItem> {
    let content_item_kinds = template
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.content_item_kinds.as_ref());
    let content = content
        .into_iter()
        .enumerate()
        .flat_map(|(index, content)| {
            let kind = content_item_kinds.map(|kinds| {
                kinds
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| ContentItemKind("unknown".to_string()))
            });
            split_oversized_text_content(&template, content)
                .into_iter()
                .map(move |content| MessageContent {
                    content,
                    kind: kind.clone(),
                })
        })
        .collect::<Vec<_>>();
    let empty_item_bytes = estimate_response_item_model_visible_bytes(&template.item(Vec::new()));
    let content_groups =
        group_content_by_model_visible_bytes(content, empty_item_bytes, |content| {
            estimate_response_item_model_visible_bytes(
                &template.item(vec![content.content.clone()]),
            )
        });
    template.into_items(content_groups)
}

fn split_agent_message(
    template: AgentMessageTemplate,
    content: Vec<AgentMessageInputContent>,
) -> Vec<ResponseItem> {
    let content = content
        .into_iter()
        .flat_map(|content| split_oversized_agent_text_content(&template, content))
        .collect::<Vec<_>>();
    let empty_item_bytes = estimate_response_item_model_visible_bytes(&template.item(Vec::new()));
    let content_groups =
        group_content_by_model_visible_bytes(content, empty_item_bytes, |content| {
            estimate_response_item_model_visible_bytes(&template.item(vec![content.clone()]))
        });
    template.into_items(content_groups)
}

fn group_content_by_model_visible_bytes<T>(
    content: Vec<T>,
    empty_item_bytes: i64,
    mut single_item_bytes: impl FnMut(&T) -> i64,
) -> Vec<Vec<T>> {
    let mut content_groups = Vec::new();
    let mut current_group = Vec::new();
    let mut current_group_bytes = empty_item_bytes;
    let max_item_bytes = i64::try_from(max_model_context_item_bytes()).unwrap_or(i64::MAX);
    for content in content {
        let single_bytes = single_item_bytes(&content);
        let candidate_bytes = if current_group.is_empty() {
            single_bytes
        } else {
            current_group_bytes
                .saturating_add(single_bytes.saturating_sub(empty_item_bytes))
                .saturating_add(1)
        };
        if candidate_bytes > max_item_bytes && !current_group.is_empty() {
            content_groups.push(std::mem::take(&mut current_group));
            current_group_bytes = single_bytes;
        } else {
            current_group_bytes = candidate_bytes;
        }
        current_group.push(content);
    }
    if !current_group.is_empty() || content_groups.is_empty() {
        content_groups.push(current_group);
    }
    content_groups
}

fn split_oversized_text_content(
    template: &MessageTemplate,
    content: ContentItem,
) -> Vec<ContentItem> {
    if estimate_item_token_count(&template.item(vec![content.clone()]))
        <= MAX_MODEL_CONTEXT_ITEM_TOKENS
    {
        return vec![content];
    }
    let (kind, text) = match content {
        ContentItem::InputText { text } => (TextContentKind::Input, text),
        ContentItem::OutputText { text } => (TextContentKind::Output, text),
        content @ (ContentItem::InputImage { .. } | ContentItem::InputAudio { .. }) => {
            return vec![content];
        }
    };

    let mut chunks = Vec::new();
    let mut remaining = text.as_str();
    while !remaining.is_empty() {
        let mut lower = 1;
        let mut upper = remaining.len().min(max_model_context_item_bytes());
        let mut best_end = None;
        while lower <= upper {
            let middle = lower + (upper - lower) / 2;
            let end = remaining.floor_char_boundary(middle);
            if end == 0 {
                lower = middle + 1;
                continue;
            }
            let candidate = kind.content(remaining[..end].to_string());
            if estimate_item_token_count(&template.item(vec![candidate]))
                <= MAX_MODEL_CONTEXT_ITEM_TOKENS
            {
                best_end = Some(end);
                lower = middle + 1;
            } else {
                upper = end - 1;
            }
        }
        let Some(end) = best_end else {
            chunks.push(kind.content(remaining.to_string()));
            break;
        };
        chunks.push(kind.content(remaining[..end].to_string()));
        remaining = &remaining[end..];
    }
    chunks
}

fn split_oversized_agent_text_content(
    template: &AgentMessageTemplate,
    content: AgentMessageInputContent,
) -> Vec<AgentMessageInputContent> {
    if estimate_item_token_count(&template.item(vec![content.clone()]))
        <= MAX_MODEL_CONTEXT_ITEM_TOKENS
    {
        return vec![content];
    }
    let AgentMessageInputContent::InputText { text } = content else {
        return vec![content];
    };

    let mut chunks = Vec::new();
    let mut remaining = text.as_str();
    while !remaining.is_empty() {
        let mut lower = 1;
        let mut upper = remaining.len().min(max_model_context_item_bytes());
        let mut best_end = None;
        while lower <= upper {
            let middle = lower + (upper - lower) / 2;
            let end = remaining.floor_char_boundary(middle);
            if end == 0 {
                lower = middle + 1;
                continue;
            }
            let candidate = AgentMessageInputContent::InputText {
                text: remaining[..end].to_string(),
            };
            if estimate_item_token_count(&template.item(vec![candidate]))
                <= MAX_MODEL_CONTEXT_ITEM_TOKENS
            {
                best_end = Some(end);
                lower = middle + 1;
            } else {
                upper = end - 1;
            }
        }
        let Some(end) = best_end else {
            chunks.push(AgentMessageInputContent::InputText {
                text: remaining.to_string(),
            });
            break;
        };
        chunks.push(AgentMessageInputContent::InputText {
            text: remaining[..end].to_string(),
        });
        remaining = &remaining[end..];
    }
    chunks
}

pub(crate) fn merge_contextual_fragments(
    fragments: Vec<Box<dyn ContextualUserFragment>>,
) -> Vec<ResponseItem> {
    let mut messages: Vec<(&str, MessageGroup, Vec<RenderedFragment>)> =
        Vec::with_capacity(fragments.len());
    for fragment in fragments {
        let group = if fragment.requires_separate_message() {
            MessageGroup::Standalone
        } else {
            MessageGroup::Mergeable
        };
        let rendered = fragment.render_fragment();
        let role = rendered.role();
        match messages.last_mut() {
            Some((previous_role, previous_group, rendered_fragments))
                if *previous_role == role
                    && *previous_group == MessageGroup::Mergeable
                    && group == MessageGroup::Mergeable =>
            {
                rendered_fragments.push(rendered);
            }
            _ => messages.push((role, group, vec![rendered])),
        }
    }
    messages
        .into_iter()
        .filter_map(|(_, _, fragments)| build_rendered_message(fragments))
        .collect()
}
