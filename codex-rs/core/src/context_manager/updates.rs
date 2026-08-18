use crate::context::ContextualUserFragment;
use codex_protocol::ResponseItemId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;

use super::history::estimate_item_token_count;

pub(super) const MAX_MODEL_CONTEXT_ITEM_TOKENS: i64 = 10_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum MessageGroup {
    Standalone,
    Mergeable,
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

    fn into_items(self, content_groups: Vec<Vec<ContentItem>>) -> Vec<ResponseItem> {
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

pub(crate) fn build_developer_update_item(text_sections: Vec<String>) -> Option<ResponseItem> {
    build_text_message("developer", text_sections)
}

pub(crate) fn build_contextual_user_message(text_sections: Vec<String>) -> Option<ResponseItem> {
    build_text_message("user", text_sections)
}

pub(crate) fn merge_contextual_fragments(
    fragments: Vec<Box<dyn ContextualUserFragment>>,
) -> Vec<ResponseItem> {
    let mut messages: Vec<(&str, MessageGroup, Vec<String>)> = Vec::with_capacity(fragments.len());
    for fragment in fragments {
        let role = fragment.role();
        let group = if fragment.requires_separate_message() {
            MessageGroup::Standalone
        } else {
            MessageGroup::Mergeable
        };
        let text = fragment.render();
        match messages.last_mut() {
            Some((previous_role, previous_group, text_sections))
                if *previous_role == role
                    && *previous_group == MessageGroup::Mergeable
                    && group == MessageGroup::Mergeable =>
            {
                text_sections.push(text);
            }
            _ => messages.push((role, group, vec![text])),
        }
    }
    messages
        .into_iter()
        .filter_map(|(role, _, text_sections)| build_text_message(role, text_sections))
        .collect()
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
    let content = content
        .into_iter()
        .flat_map(|content| split_oversized_text_content(&template, content))
        .collect::<Vec<_>>();
    let mut content_groups = Vec::new();
    let mut current_group = Vec::new();
    for content in content {
        current_group.push(content);
        if estimate_item_token_count(&template.item(current_group.clone()))
            > MAX_MODEL_CONTEXT_ITEM_TOKENS
            && current_group.len() > 1
        {
            let overflow = current_group.remove(current_group.len() - 1);
            content_groups.push(std::mem::take(&mut current_group));
            current_group.push(overflow);
        }
    }
    if !current_group.is_empty() || content_groups.is_empty() {
        content_groups.push(current_group);
    }
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
    let mut content_groups = Vec::new();
    let mut current_group = Vec::new();
    for content in content {
        current_group.push(content);
        if estimate_item_token_count(&template.item(current_group.clone()))
            > MAX_MODEL_CONTEXT_ITEM_TOKENS
            && current_group.len() > 1
        {
            let overflow = current_group.remove(current_group.len() - 1);
            content_groups.push(std::mem::take(&mut current_group));
            current_group.push(overflow);
        }
    }
    if !current_group.is_empty() || content_groups.is_empty() {
        content_groups.push(current_group);
    }
    template.into_items(content_groups)
}

fn build_text_message(role: &str, text_sections: Vec<String>) -> Option<ResponseItem> {
    if text_sections.is_empty() {
        return None;
    }

    Some(ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: text_sections
            .into_iter()
            .map(|text| ContentItem::InputText { text })
            .collect(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
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
        let mut upper = remaining.len();
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
        let mut upper = remaining.len();
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
