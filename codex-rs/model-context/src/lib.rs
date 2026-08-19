//! Shared estimation of model-visible response item size.

use std::num::NonZeroUsize;
use std::sync::LazyLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use codex_utils_audio::estimate_audio_token_count;
use codex_utils_cache::BlockingLruCache;
use codex_utils_cache::sha1_digest;

use codex_utils_string::approx_bytes_for_tokens;
use codex_utils_string::approx_tokens_from_byte_count;

const RESIZED_IMAGE_BYTES_ESTIMATE: i64 = 7_373;
const ORIGINAL_IMAGE_PATCH_SIZE: u32 = 32;
const ORIGINAL_IMAGE_MAX_PATCHES: usize = 10_000;
const ORIGINAL_IMAGE_ESTIMATE_CACHE_SIZE: usize = 32;

static ORIGINAL_IMAGE_ESTIMATE_CACHE: LazyLock<BlockingLruCache<[u8; 20], Option<i64>>> =
    LazyLock::new(|| {
        BlockingLruCache::new(
            NonZeroUsize::new(ORIGINAL_IMAGE_ESTIMATE_CACHE_SIZE).unwrap_or(NonZeroUsize::MIN),
        )
    });

/// Returns the model-visible byte estimate used to enforce per-item and total request limits.
pub fn estimate_response_item_model_visible_bytes(item: &ResponseItem) -> i64 {
    match item {
        item @ ResponseItem::Reasoning {
            encrypted_content: Some(content),
            ..
        }
        | item @ ResponseItem::Compaction {
            encrypted_content: content,
            ..
        }
        | item @ ResponseItem::ContextCompaction {
            encrypted_content: Some(content),
            ..
        } => serialized_response_item_bytes(item)
            .saturating_sub(i64::try_from(content.len()).unwrap_or(i64::MAX))
            .saturating_add(
                i64::try_from(estimate_reasoning_length(content.len())).unwrap_or(i64::MAX),
            ),
        item => {
            let raw = serialized_response_item_bytes(item);
            let (image_payload_bytes, image_replacement_bytes) =
                image_data_url_estimate_adjustment(item);
            let (audio_payload_bytes, audio_replacement_bytes) =
                audio_data_url_estimate_adjustment(item);
            let (encrypted_payload_bytes, encrypted_replacement_bytes) =
                encrypted_function_output_estimate_adjustment(item);
            let raw = raw
                .saturating_sub(image_payload_bytes)
                .saturating_add(image_replacement_bytes)
                .saturating_sub(audio_payload_bytes)
                .saturating_add(audio_replacement_bytes);
            raw.saturating_sub(encrypted_payload_bytes)
                .saturating_add(encrypted_replacement_bytes)
        }
    }
}

/// Returns the coarse token estimate corresponding to the model-visible byte estimate.
pub fn estimate_response_item_token_count(item: &ResponseItem) -> i64 {
    let bytes = estimate_response_item_model_visible_bytes(item);
    if bytes <= 0 {
        return 0;
    }
    i64::try_from(approx_tokens_from_byte_count(
        usize::try_from(bytes).unwrap_or(usize::MAX),
    ))
    .unwrap_or(i64::MAX)
}

fn estimate_reasoning_length(encoded_len: usize) -> usize {
    encoded_len
        .saturating_mul(3)
        .checked_div(4)
        .unwrap_or(0)
        .saturating_sub(650)
}

fn estimate_encrypted_function_output_length(encoded_len: usize) -> usize {
    encoded_len.saturating_mul(9).div_ceil(16)
}

fn serialized_response_item_bytes(item: &ResponseItem) -> i64 {
    serde_json::to_string(item)
        .map(|serialized| i64::try_from(serialized.len()).unwrap_or(i64::MAX))
        .unwrap_or(i64::MAX)
}

fn parse_base64_data_url<'a>(url: &'a str, media_type_prefix: &str) -> Option<&'a str> {
    if !url
        .get(.."data:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        return None;
    }
    let comma_index = url.find(',')?;
    let metadata = &url[..comma_index];
    let payload = &url[comma_index + 1..];
    let metadata_without_scheme = &metadata["data:".len()..];
    let mut metadata_parts = metadata_without_scheme.split(';');
    let mime_type = metadata_parts.next().unwrap_or_default();
    let has_base64_marker = metadata_parts.any(|part| part.eq_ignore_ascii_case("base64"));
    if !mime_type
        .get(..media_type_prefix.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(media_type_prefix))
        || !has_base64_marker
    {
        return None;
    }
    Some(payload)
}

fn estimate_original_image_bytes(image_url: &str) -> Option<i64> {
    let key = sha1_digest(image_url.as_bytes());
    ORIGINAL_IMAGE_ESTIMATE_CACHE.get_or_insert_with(key, || {
        let payload = match parse_base64_data_url(image_url, "image/") {
            Some(payload) => payload,
            None => {
                tracing::trace!("skipping original-detail estimate for non-base64 image data URL");
                return None;
            }
        };
        let bytes = match BASE64_STANDARD.decode(payload) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::trace!("failed to decode original-detail image payload: {error}");
                return None;
            }
        };
        let dynamic = match image::load_from_memory(&bytes) {
            Ok(dynamic) => dynamic,
            Err(error) => {
                tracing::trace!("failed to decode original-detail image bytes: {error}");
                return None;
            }
        };
        let width = i64::from(dynamic.width());
        let height = i64::from(dynamic.height());
        let patch_size = i64::from(ORIGINAL_IMAGE_PATCH_SIZE);
        let patches_wide = width.saturating_add(patch_size.saturating_sub(1)) / patch_size;
        let patches_high = height.saturating_add(patch_size.saturating_sub(1)) / patch_size;
        let patch_count = patches_wide.saturating_mul(patches_high);
        let patch_count = usize::try_from(patch_count).unwrap_or(usize::MAX);
        let patch_count = patch_count.min(ORIGINAL_IMAGE_MAX_PATCHES);
        Some(i64::try_from(approx_bytes_for_tokens(patch_count)).unwrap_or(i64::MAX))
    })
}

fn image_data_url_estimate_adjustment(item: &ResponseItem) -> (i64, i64) {
    let mut payload_bytes = 0i64;
    let mut replacement_bytes = 0i64;
    let mut accumulate = |image_url: &str, detail: Option<ImageDetail>| {
        if let Some(payload_len) = parse_base64_data_url(image_url, "image/").map(str::len) {
            payload_bytes =
                payload_bytes.saturating_add(i64::try_from(payload_len).unwrap_or(i64::MAX));
            replacement_bytes = replacement_bytes.saturating_add(match detail {
                Some(ImageDetail::Original) => {
                    estimate_original_image_bytes(image_url).unwrap_or(RESIZED_IMAGE_BYTES_ESTIMATE)
                }
                _ => RESIZED_IMAGE_BYTES_ESTIMATE,
            });
        }
    };

    match item {
        ResponseItem::Message { content, .. } => {
            for content_item in content {
                if let ContentItem::InputImage { image_url, detail } = content_item {
                    accumulate(image_url, *detail);
                }
            }
        }
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            if let FunctionCallOutputBody::ContentItems(items) = &output.body {
                for content_item in items {
                    if let FunctionCallOutputContentItem::InputImage { image_url, detail } =
                        content_item
                    {
                        accumulate(image_url, *detail);
                    }
                }
            }
        }
        _ => {}
    }
    (payload_bytes, replacement_bytes)
}

fn audio_data_url_estimate_adjustment(item: &ResponseItem) -> (i64, i64) {
    let mut payload_bytes = 0i64;
    let mut replacement_bytes = 0i64;
    let mut accumulate = |audio_url: &str| {
        if let Some(payload_len) = parse_base64_data_url(audio_url, "audio/").map(str::len) {
            payload_bytes =
                payload_bytes.saturating_add(i64::try_from(payload_len).unwrap_or(i64::MAX));
            replacement_bytes = replacement_bytes.saturating_add(
                i64::try_from(approx_bytes_for_tokens(estimate_audio_token_count(
                    audio_url,
                )))
                .unwrap_or(i64::MAX),
            );
        }
    };

    match item {
        ResponseItem::Message { content, .. } => {
            for content_item in content {
                if let ContentItem::InputAudio { audio_url } = content_item {
                    accumulate(audio_url);
                }
            }
        }
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            if let FunctionCallOutputBody::ContentItems(items) = &output.body {
                for content_item in items {
                    if let FunctionCallOutputContentItem::InputAudio { audio_url } = content_item {
                        accumulate(audio_url);
                    }
                }
            }
        }
        _ => {}
    }
    (payload_bytes, replacement_bytes)
}

fn encrypted_function_output_estimate_adjustment(item: &ResponseItem) -> (i64, i64) {
    let mut payload_bytes = 0i64;
    let mut replacement_bytes = 0i64;
    let mut accumulate = |encrypted_content: &str| {
        payload_bytes = payload_bytes
            .saturating_add(i64::try_from(encrypted_content.len()).unwrap_or(i64::MAX));
        replacement_bytes = replacement_bytes.saturating_add(
            i64::try_from(estimate_encrypted_function_output_length(
                encrypted_content.len(),
            ))
            .unwrap_or(i64::MAX),
        );
    };

    match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            if let FunctionCallOutputBody::ContentItems(items) = &output.body {
                for item in items {
                    if let FunctionCallOutputContentItem::EncryptedContent { encrypted_content } =
                        item
                    {
                        accumulate(encrypted_content);
                    }
                }
            }
        }
        ResponseItem::AgentMessage { content, .. } => {
            for item in content {
                if let AgentMessageInputContent::EncryptedContent { encrypted_content } = item {
                    accumulate(encrypted_content);
                }
            }
        }
        _ => {}
    }
    (payload_bytes, replacement_bytes)
}

#[cfg(test)]
#[path = "model_visible_tests.rs"]
mod tests;
