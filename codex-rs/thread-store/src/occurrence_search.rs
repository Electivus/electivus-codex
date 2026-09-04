use std::borrow::Cow;

use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::UserInput;
use codex_protocol::protocol::strip_user_message_prefix;
use pulldown_cmark::Event;
use pulldown_cmark::Parser;
use pulldown_cmark::TagEnd;

use crate::SearchTextRange;
use crate::StoredThreadOccurrence;

const SNIPPET_CONTEXT_BEFORE_CHARS: usize = 48;
const SNIPPET_CONTEXT_AFTER_CHARS: usize = 96;

pub(crate) fn searchable_text(item: &ThreadItem) -> Option<Cow<'_, str>> {
    match item {
        ThreadItem::UserMessage { content, .. } => {
            let mut text_parts = content
                .iter()
                .filter_map(|input| match input {
                    UserInput::Text { text, .. } => Some(strip_user_message_prefix(text)),
                    UserInput::Image { .. }
                    | UserInput::LocalImage { .. }
                    | UserInput::Audio { .. }
                    | UserInput::LocalAudio { .. }
                    | UserInput::Skill { .. }
                    | UserInput::Mention { .. } => None,
                })
                .filter(|text| !text.is_empty())
                .peekable();
            let first = text_parts.next()?;
            match text_parts.next() {
                None => Some(Cow::Borrowed(first)),
                Some(second) => {
                    let mut parts = vec![first, second];
                    parts.extend(text_parts);
                    Some(Cow::Owned(parts.concat()))
                }
            }
        }
        ThreadItem::AgentMessage { text, .. } => {
            let text = markdown_to_search_text(text);
            (!text.is_empty()).then_some(Cow::Owned(text))
        }
        // Only user and agent messages contribute searchable text. New item kinds remain
        // non-searchable until their text semantics are defined explicitly.
        _ => None,
    }
}

fn markdown_to_search_text(markdown: &str) -> String {
    let mut text = String::new();
    for event in Parser::new(markdown.trim()) {
        match event {
            Event::Text(value)
            | Event::Code(value)
            | Event::Html(value)
            | Event::InlineHtml(value) => text.push_str(&value),
            Event::SoftBreak | Event::HardBreak | Event::Rule => text.push(' '),
            Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::BlockQuote
                | TagEnd::CodeBlock
                | TagEnd::List(_)
                | TagEnd::Item
                | TagEnd::Table
                | TagEnd::TableHead
                | TagEnd::TableRow
                | TagEnd::TableCell,
            ) => text.push(' '),
            Event::Start(_)
            | Event::End(
                TagEnd::Emphasis
                | TagEnd::Strong
                | TagEnd::Strikethrough
                | TagEnd::Link
                | TagEnd::HtmlBlock
                | TagEnd::FootnoteDefinition
                | TagEnd::Image
                | TagEnd::MetadataBlock(_),
            )
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_) => {}
        }
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) struct LiteralMatcher {
    lowercase_needle: String,
}

impl LiteralMatcher {
    pub(crate) fn new(needle: &str) -> Self {
        Self {
            lowercase_needle: needle.to_lowercase(),
        }
    }

    pub(crate) fn find_ranges(&self, text: &str, limit: usize) -> Vec<std::ops::Range<usize>> {
        let lowercase_text = text.to_lowercase();
        let mut spans = Vec::with_capacity(text.chars().count());
        let mut lowercase_start = 0;
        for (original_start, character) in text.char_indices() {
            let lowercase_end =
                lowercase_start + character.to_lowercase().map(char::len_utf8).sum::<usize>();
            spans.push((
                lowercase_start..lowercase_end,
                original_start..original_start + character.len_utf8(),
            ));
            lowercase_start = lowercase_end;
        }

        // Map lowercase matches back to original byte ranges in one linear pass.
        let mut start_span = 0;
        let mut end_span = 0;
        lowercase_text
            .match_indices(self.lowercase_needle.as_str())
            .take(limit)
            .filter_map(|(start, matched)| {
                let end = start.saturating_add(matched.len());
                while spans.get(start_span)?.0.end <= start {
                    start_span += 1;
                }
                while spans.get(end_span)?.0.end <= end.saturating_sub(1) {
                    end_span += 1;
                }
                let original_start = spans.get(start_span)?.1.start;
                let original_end = spans.get(end_span)?.1.end;
                Some(original_start..original_end)
            })
            .collect()
    }
}

pub(crate) fn occurrence_in_item(
    turn_id: &str,
    item_id: &str,
    text: &str,
    matched: std::ops::Range<usize>,
    turn_cursor: &str,
) -> StoredThreadOccurrence {
    let snippet_start = char_start_before(text, matched.start, SNIPPET_CONTEXT_BEFORE_CHARS);
    let snippet_end = char_end_after(text, matched.end, SNIPPET_CONTEXT_AFTER_CHARS);
    let leading_ellipsis = snippet_start > 0;
    let trailing_ellipsis = snippet_end < text.len();
    let mut snippet = String::new();
    if leading_ellipsis {
        snippet.push_str("... ");
    }
    snippet.push_str(&text[snippet_start..snippet_end]);
    if trailing_ellipsis {
        snippet.push_str(" ...");
    }
    let snippet_match_start =
        if leading_ellipsis { 4 } else { 0 } + utf16_len(&text[snippet_start..matched.start]);
    let match_len = utf16_len(&text[matched]);

    StoredThreadOccurrence {
        turn_id: turn_id.to_string(),
        item_id: item_id.to_string(),
        snippet,
        snippet_match_range: SearchTextRange {
            start: snippet_match_start,
            end: snippet_match_start.saturating_add(match_len),
        },
        turn_cursor: turn_cursor.to_string(),
    }
}

fn utf16_len(text: &str) -> u32 {
    u32::try_from(text.encode_utf16().count()).unwrap_or(u32::MAX)
}

fn char_start_before(text: &str, byte_index: usize, chars_before: usize) -> usize {
    text[..byte_index]
        .char_indices()
        .rev()
        .nth(chars_before)
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn char_end_after(text: &str, byte_index: usize, chars_after: usize) -> usize {
    text[byte_index..]
        .char_indices()
        .nth(chars_after)
        .map(|(offset, _)| byte_index.saturating_add(offset))
        .unwrap_or(text.len())
}
