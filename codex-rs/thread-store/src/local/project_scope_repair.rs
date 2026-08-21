//! Bounded local repair before Project Session Scope queries.
//!
//! The interface hides filesystem traversal, reconciliation, DB union queries, and exact-cwd
//! fallback so callers cannot accidentally mix filesystem and DB cursors.

use codex_rollout::RolloutConfig;
use codex_rollout::RolloutRecorder;
use tracing::debug;
use tracing::warn;

use crate::ListThreadsParams;
use crate::ThreadLocationFilter;
use crate::ThreadRelationFilter;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

// Match the rollout scanner's existing per-request hard cap so Project Scope repair can perform
// one complete bounded pass without repeatedly walking files before an internal cursor.
const REPAIR_SCAN_BUDGET: usize = 10_000;
// A single rollout can contain long tool sessions, while the request-wide cap prevents a project
// listing from retaining or parsing an unbounded aggregate across historical exact-cwd sessions.
const REPAIR_ROLLOUT_SOURCE_BYTE_BUDGET: u64 = 32 * 1024 * 1024;
const REPAIR_TOTAL_SOURCE_BYTE_BUDGET: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
struct RepairSweepLimits {
    scan_budget: usize,
    rollout_source_byte_budget: u64,
    total_source_byte_budget: u64,
}

const DEFAULT_REPAIR_SWEEP_LIMITS: RepairSweepLimits = RepairSweepLimits {
    scan_budget: REPAIR_SCAN_BUDGET,
    rollout_source_byte_budget: REPAIR_ROLLOUT_SOURCE_BYTE_BUDGET,
    total_source_byte_budget: REPAIR_TOTAL_SOURCE_BYTE_BUDGET,
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn list_threads(
    state_db: Option<codex_rollout::StateDbHandle>,
    config: &RolloutConfig,
    default_model_provider_id: &str,
    params: &ListThreadsParams,
    cursor: Option<&codex_rollout::Cursor>,
    sort_key: codex_rollout::ThreadSortKey,
    sort_direction: codex_rollout::SortDirection,
) -> ThreadStoreResult<codex_rollout::ThreadsPage> {
    list_threads_with_limits(
        state_db,
        config,
        default_model_provider_id,
        params,
        cursor,
        sort_key,
        sort_direction,
        DEFAULT_REPAIR_SWEEP_LIMITS,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn list_threads_with_limits(
    state_db: Option<codex_rollout::StateDbHandle>,
    config: &RolloutConfig,
    default_model_provider_id: &str,
    params: &ListThreadsParams,
    cursor: Option<&codex_rollout::Cursor>,
    sort_key: codex_rollout::ThreadSortKey,
    sort_direction: codex_rollout::SortDirection,
    limits: RepairSweepLimits,
) -> ThreadStoreResult<codex_rollout::ThreadsPage> {
    let ThreadLocationFilter::ProjectSessionScope {
        cwd,
        repository_identity,
    } = &params.location_filter
    else {
        return Err(ThreadStoreError::Internal {
            message: "project repair received a non-project location filter".to_string(),
        });
    };

    // Relation and section filters are DB-owned. They bypass file listing, and
    // repairing first would both expand this feature's scope and reject otherwise queryable rows.
    if params.use_state_db_only || params.relation_filter.is_some() || params.section.is_some() {
        return query_project_threads(
            state_db.as_deref(),
            config,
            params,
            cursor,
            sort_key,
            sort_direction,
            cwd,
            repository_identity,
        )
        .await
        .map(Into::into)
        .ok_or_else(state_db_unavailable_error);
    }

    let Some(state_db) = state_db else {
        return fallback_to_exact_cwd(
            config,
            default_model_provider_id,
            params,
            cursor,
            sort_key,
            sort_direction,
            cwd,
            limits,
            /*previously_scanned_files*/ 0,
        )
        .await;
    };

    let repair_outcome = repair_exact_cwd_rollouts(
        &state_db,
        config,
        default_model_provider_id,
        cwd,
        params.archived,
        limits,
    )
    .await?;
    debug!(
        scanned_files = repair_outcome.scanned_files,
        source_bytes = repair_outcome.source_bytes,
        "project repair sweep completed"
    );

    if let Some(page) = query_project_threads(
        Some(state_db.as_ref()),
        config,
        params,
        cursor,
        sort_key,
        sort_direction,
        cwd,
        repository_identity,
    )
    .await
    {
        return Ok(page.into());
    }
    fallback_to_exact_cwd(
        config,
        default_model_provider_id,
        params,
        cursor,
        sort_key,
        sort_direction,
        cwd,
        limits,
        repair_outcome.scanned_files,
    )
    .await
}

struct RepairSweepOutcome {
    scanned_files: usize,
    source_bytes: u64,
}

async fn repair_exact_cwd_rollouts(
    state_db: &codex_rollout::StateDbHandle,
    config: &RolloutConfig,
    default_model_provider_id: &str,
    cwd: &std::path::Path,
    archived: bool,
    limits: RepairSweepLimits,
) -> ThreadStoreResult<RepairSweepOutcome> {
    if limits.scan_budget == 0
        || limits.rollout_source_byte_budget == 0
        || limits.total_source_byte_budget == 0
    {
        return Err(repair_budget_error(
            /*scanned_files*/ 0,
            limits.scan_budget,
        ));
    }

    let cwd_filter = cwd.to_path_buf();
    let cwd_filters = std::slice::from_ref(&cwd_filter);
    let page = if archived {
        codex_rollout::get_threads_in_root(
            config
                .codex_home
                .join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR),
            limits.scan_budget,
            /*cursor*/ None,
            codex_rollout::ThreadSortKey::CreatedAt,
            codex_rollout::ThreadListConfig {
                allowed_sources: &[],
                model_providers: None,
                cwd_filters: Some(cwd_filters),
                default_provider: default_model_provider_id,
                layout: codex_rollout::ThreadListLayout::Flat,
            },
        )
        .await
    } else {
        codex_rollout::get_threads(
            config.codex_home.as_path(),
            limits.scan_budget,
            /*cursor*/ None,
            codex_rollout::ThreadSortKey::CreatedAt,
            &[],
            /*model_providers*/ None,
            Some(cwd_filters),
            default_model_provider_id,
        )
        .await
    }
    .map_err(|err| {
        warn!(error = %err, "project repair could not scan exact-cwd rollouts");
        ThreadStoreError::Internal {
            message: "project repair could not scan rollouts".to_string(),
        }
    })?;

    let scanned_files = page.num_scanned_files;
    if page.reached_scan_cap || page.next_cursor.is_some() || scanned_files > limits.scan_budget {
        return Err(repair_budget_error(scanned_files, limits.scan_budget));
    }

    let mut source_bytes = 0_u64;
    for item in &page.items {
        let remaining_source_bytes = limits.total_source_byte_budget.saturating_sub(source_bytes);
        if remaining_source_bytes == 0 {
            return Err(repair_source_error());
        }
        let maximum_source_bytes = limits
            .rollout_source_byte_budget
            .min(remaining_source_bytes);
        let consumed_source_bytes = match codex_rollout::state_db::reconcile_rollout_checked(
            state_db.as_ref(),
            item.path.as_path(),
            default_model_provider_id,
            Some(archived),
            maximum_source_bytes,
        )
        .await
        {
            Ok(source_bytes) => source_bytes,
            Err(err) => {
                warn!(
                    rollout_path = %item.path.display(),
                    error = %err,
                    "project repair could not reconcile rollout"
                );
                return Err(repair_source_error());
            }
        };
        source_bytes = source_bytes.saturating_add(consumed_source_bytes);
    }

    Ok(RepairSweepOutcome {
        scanned_files,
        source_bytes,
    })
}

#[allow(clippy::too_many_arguments)]
async fn query_project_threads(
    state_db: Option<&codex_state::StateRuntime>,
    config: &RolloutConfig,
    params: &ListThreadsParams,
    cursor: Option<&codex_rollout::Cursor>,
    sort_key: codex_rollout::ThreadSortKey,
    sort_direction: codex_rollout::SortDirection,
    cwd: &std::path::Path,
    repository_identity: &str,
) -> Option<codex_state::ThreadsPage> {
    let relation_filter = params
        .relation_filter
        .map(|relation_filter| match relation_filter {
            ThreadRelationFilter::DirectChildrenOf(parent_thread_id) => {
                codex_state::ThreadRelationFilter::DirectChildrenOf(parent_thread_id)
            }
            ThreadRelationFilter::DescendantsOf(ancestor_thread_id) => {
                codex_state::ThreadRelationFilter::DescendantsOf(ancestor_thread_id)
            }
        });
    let cwd_filter = cwd.to_path_buf();
    codex_rollout::state_db::list_threads_db(
        state_db,
        &config.sqlite,
        params.page_size,
        cursor,
        sort_key,
        sort_direction,
        params.allowed_sources.as_slice(),
        params.model_providers.as_deref(),
        Some(std::slice::from_ref(&cwd_filter)),
        Some(repository_identity),
        relation_filter,
        params.archived,
        params.section.as_ref().map(Option::as_deref),
        params.search_term.as_deref(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn fallback_to_exact_cwd(
    config: &RolloutConfig,
    default_model_provider_id: &str,
    params: &ListThreadsParams,
    cursor: Option<&codex_rollout::Cursor>,
    sort_key: codex_rollout::ThreadSortKey,
    sort_direction: codex_rollout::SortDirection,
    cwd: &std::path::Path,
    limits: RepairSweepLimits,
    previously_scanned_files: usize,
) -> ThreadStoreResult<codex_rollout::ThreadsPage> {
    // Legacy Asc and search listing may rescan from the root. Keep the strict Project Scope
    // fallback bounded by allowing only a single Desc page without search.
    if params.relation_filter.is_some()
        || params.section.is_some()
        || matches!(sort_direction, codex_rollout::SortDirection::Asc)
        || params.search_term.is_some()
    {
        return Err(state_db_unavailable_error());
    }
    let cwd_filter = cwd.to_path_buf();
    let cwd_filters = Some(std::slice::from_ref(&cwd_filter));
    let page = if params.archived {
        RolloutRecorder::list_archived_threads(
            /*state_db_ctx*/ None,
            config,
            params.page_size,
            cursor,
            sort_key,
            sort_direction,
            params.allowed_sources.as_slice(),
            params.model_providers.as_deref(),
            cwd_filters,
            default_model_provider_id,
            params.search_term.as_deref(),
        )
        .await
    } else {
        RolloutRecorder::list_threads(
            /*state_db_ctx*/ None,
            config,
            params.page_size,
            cursor,
            sort_key,
            sort_direction,
            params.allowed_sources.as_slice(),
            params.model_providers.as_deref(),
            cwd_filters,
            default_model_provider_id,
            params.search_term.as_deref(),
        )
        .await
    };
    let mut page = page.map_err(|err| {
        warn!(error = %err, "project fallback could not list exact-cwd rollouts");
        ThreadStoreError::Internal {
            message: "project fallback could not list rollouts".to_string(),
        }
    })?;
    let total_scanned_files = previously_scanned_files.saturating_add(page.num_scanned_files);
    if page.reached_scan_cap || total_scanned_files > limits.scan_budget {
        return Err(repair_budget_error(total_scanned_files, limits.scan_budget));
    }
    page.num_scanned_files = total_scanned_files;
    Ok(page)
}

fn state_db_unavailable_error() -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: "state DB unavailable for filtered thread listing".to_string(),
    }
}

fn repair_budget_error(scanned_files: usize, scan_budget: usize) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!(
            "project repair scan budget exceeded: scanned {scanned_files} files with budget \
             {scan_budget}"
        ),
    }
}

fn repair_source_error() -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: "project repair could not reconcile a rollout".to_string(),
    }
}

#[cfg(test)]
#[path = "project_scope_repair_tests.rs"]
mod tests;
