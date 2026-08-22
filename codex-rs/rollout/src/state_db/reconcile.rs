//! Checked and best-effort rollout reconciliation into runtime state.

use codex_protocol::protocol::ThreadHistoryMode;
use codex_state::ThreadMetadataBuilder;
use std::path::Path;
use tracing::warn;

use crate::RolloutItem;
use crate::metadata;
use crate::recorder::RolloutRecorder;

use super::apply_rollout_items;
use super::normalize_cwd_for_state_db;

/// Reconcile a complete rollout into runtime state under a decoded source-byte limit.
pub async fn reconcile_rollout_checked(
    context: &codex_state::StateRuntime,
    rollout_path: &Path,
    default_provider: &str,
    archived_only: Option<bool>,
    maximum_source_bytes: u64,
) -> anyhow::Result<u64> {
    let (lines, _thread_id, parse_errors, source_bytes) =
        RolloutRecorder::load_rollout_lines_bounded(rollout_path, maximum_source_bytes).await?;
    if parse_errors != 0 {
        anyhow::bail!("rollout contains {parse_errors} invalid record(s)");
    }
    let outcome = metadata::extract_metadata_from_items(
        rollout_path,
        lines.iter().map(|line| &line.item),
        parse_errors,
        default_provider,
    )
    .await?;
    persist_extracted_rollout(context, outcome, archived_only).await?;
    Ok(source_bytes)
}

async fn persist_extracted_rollout(
    context: &codex_state::StateRuntime,
    outcome: codex_state::ExtractionOutcome,
    archived_only: Option<bool>,
) -> anyhow::Result<()> {
    let mut metadata = outcome.metadata;
    let memory_mode = outcome.memory_mode.unwrap_or_else(|| "enabled".to_string());
    metadata.cwd = normalize_cwd_for_state_db(&metadata.cwd);
    let existing_metadata = context.get_thread(metadata.id).await?;
    // A fallback scan cannot distinguish an obsolete immutable rollout from the rollout
    // currently selected after thread/revert, so it must not replace that selected path.
    if existing_metadata
        .as_ref()
        .is_some_and(|existing| existing.rollout_path != metadata.rollout_path)
    {
        return Ok(());
    }
    // Paginated metadata updates are SQLite-only. Use the rollout mode to seed a
    // missing row, then keep the value from SQLite.
    let restore_memory_mode_from_rollout =
        existing_metadata.is_none() || matches!(metadata.history_mode, ThreadHistoryMode::Legacy);
    if let Some(existing_metadata) = existing_metadata.as_ref() {
        metadata.prefer_existing_git_info(existing_metadata);
        metadata.prefer_existing_explicit_title(existing_metadata);
    }
    match archived_only {
        Some(true) if metadata.archived_at.is_none() => {
            metadata.archived_at = Some(metadata.updated_at);
        }
        Some(false) => metadata.archived_at = None,
        Some(true) | None => {}
    }
    context.upsert_thread(&metadata).await?;
    if restore_memory_mode_from_rollout
        && !context
            .set_thread_memory_mode(metadata.id, memory_mode.as_str())
            .await?
    {
        anyhow::bail!("reconciled thread is unavailable for memory mode update");
    }
    Ok(())
}

/// Reconcile rollout items into runtime state, falling back to a best-effort rollout scan.
pub async fn reconcile_rollout(
    context: Option<&codex_state::StateRuntime>,
    rollout_path: &Path,
    default_provider: &str,
    builder: Option<&ThreadMetadataBuilder>,
    items: &[RolloutItem],
    archived_only: Option<bool>,
    new_thread_memory_mode: Option<&str>,
) {
    let Some(context) = context else {
        return;
    };
    if builder.is_some() || !items.is_empty() {
        apply_rollout_items(
            Some(context),
            rollout_path,
            default_provider,
            builder,
            items,
            "reconcile_rollout",
            new_thread_memory_mode,
            /*updated_at_override*/ None,
        )
        .await;
        return;
    }
    // Preserve the legacy best-effort behavior here: the unbounded extractor
    // tolerates malformed non-metadata records while reporting their count.
    // Callers that require complete reconciliation use the checked seam above.
    let result = match metadata::extract_metadata_from_rollout(rollout_path, default_provider).await
    {
        Ok(outcome) => persist_extracted_rollout(context, outcome, archived_only).await,
        Err(err) => Err(err),
    };
    if let Err(err) = result {
        warn!(
            "state db reconcile_rollout failed {}: {err}",
            rollout_path.display()
        );
    }
}
