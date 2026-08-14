//! Shared SQLite thread-list query construction.

use super::*;
use crate::SortDirection;

pub(super) fn push_thread_select_columns(builder: &mut QueryBuilder<Sqlite>) {
    builder.push(
        r#"
SELECT
    threads.id,
    threads.rollout_path,
    threads.created_at_ms AS created_at,
    threads.updated_at_ms AS updated_at,
    threads.recency_at_ms AS recency_at,
    threads.source,
    threads.history_mode,
    threads.thread_source,
    threads.agent_nickname,
    threads.agent_role,
    threads.agent_path,
    threads.model_provider,
    threads.model,
    threads.reasoning_effort,
    threads.cwd,
    threads.cli_version,
    threads.title,
    threads.name,
    threads.preview,
    threads.sandbox_policy,
    threads.approval_mode,
    threads.tokens_used,
    threads.first_user_message,
    threads.archived_at,
    threads.thread_section_id AS section,
    (
        SELECT thread_sections.name
        FROM thread_sections
        WHERE thread_sections.id = threads.thread_section_id
    ) AS section_name,
    (
        SELECT thread_sections.appearance
        FROM thread_sections
        WHERE thread_sections.id = threads.thread_section_id
    ) AS section_appearance,
    threads.section_position,
    threads.section_entered_at_ms,
    threads.git_sha,
    threads.git_branch,
    threads.git_origin_url,
    threads.repository_identity,
    threads.git_origin_url_is_explicit
"#,
    );
}

#[derive(Clone, Copy)]
pub struct ThreadFilterOptions<'a> {
    pub archived_only: bool,
    pub allowed_sources: &'a [String],
    pub model_providers: Option<&'a [String]>,
    pub cwd_filters: Option<&'a [PathBuf]>,
    pub repository_identity: Option<&'a str>,
    pub section: Option<Option<&'a str>>,
    pub anchor: Option<&'a crate::Anchor>,
    pub sort_key: SortKey,
    pub sort_direction: SortDirection,
    pub search_term: Option<&'a str>,
}

pub(in crate::runtime) fn push_thread_filters<'a>(
    builder: &mut QueryBuilder<Sqlite>,
    options: ThreadFilterOptions<'a>,
    include_thread_id_tiebreaker: bool,
) {
    push_thread_filters_with_preview(
        builder,
        options,
        include_thread_id_tiebreaker,
        /*include_empty_preview*/ false,
    );
}

pub(super) fn push_thread_filters_with_preview<'a>(
    builder: &mut QueryBuilder<Sqlite>,
    options: ThreadFilterOptions<'a>,
    include_thread_id_tiebreaker: bool,
    include_empty_preview: bool,
) {
    let ThreadFilterOptions {
        archived_only,
        allowed_sources,
        model_providers,
        cwd_filters,
        repository_identity,
        section,
        anchor,
        sort_key,
        sort_direction,
        search_term,
    } = options;
    builder.push(" WHERE 1 = 1");
    if archived_only {
        builder.push(" AND threads.archived = 1");
    } else {
        builder.push(" AND threads.archived = 0");
    }
    if !include_empty_preview {
        builder.push(" AND threads.preview <> ''");
    }
    match section {
        Some(Some(section)) => {
            builder.push(" AND threads.thread_section_id = ");
            builder.push_bind(section);
        }
        Some(None) => {
            builder.push(" AND threads.thread_section_id IS NULL");
        }
        None => {}
    }
    if !allowed_sources.is_empty() {
        builder.push(" AND threads.source IN (");
        let mut separated = builder.separated(", ");
        for source in allowed_sources {
            separated.push_bind(source);
        }
        separated.push_unseparated(")");
    }
    if let Some(model_providers) = model_providers
        && !model_providers.is_empty()
    {
        builder.push(" AND threads.model_provider IN (");
        let mut separated = builder.separated(", ");
        for provider in model_providers {
            separated.push_bind(provider);
        }
        separated.push_unseparated(")");
    }
    match (cwd_filters, repository_identity) {
        (Some([]), None) => {
            builder.push(" AND 1 = 0");
        }
        (Some(cwd_filters), Some(repository_identity)) => {
            builder.push(" AND (threads.cwd IN (");
            let mut separated = builder.separated(", ");
            for cwd in cwd_filters {
                separated.push_bind(cwd.display().to_string());
            }
            separated.push_unseparated(") OR threads.repository_identity = ");
            builder.push_bind(repository_identity);
            builder.push(")");
        }
        (None, Some(repository_identity)) => {
            builder.push(" AND threads.repository_identity = ");
            builder.push_bind(repository_identity);
        }
        (Some(cwd_filters), None) => {
            builder.push(" AND threads.cwd IN (");
            let mut separated = builder.separated(", ");
            for cwd in cwd_filters {
                separated.push_bind(cwd.display().to_string());
            }
            separated.push_unseparated(")");
        }
        (None, None) => {}
    }
    if let Some(search_term) = search_term {
        builder.push(" AND (instr(COALESCE(threads.name, ''), ");
        builder.push_bind(search_term);
        builder.push(") > 0 OR instr(threads.title, ");
        builder.push_bind(search_term);
        builder.push(") > 0 OR instr(threads.preview, ");
        builder.push_bind(search_term);
        builder.push(") > 0)");
    }
    if let Some(anchor) = anchor {
        let anchor_ts = datetime_to_epoch_millis(anchor.ts);
        let column = match sort_key {
            SortKey::CreatedAt => "threads.created_at_ms",
            SortKey::UpdatedAt => "threads.updated_at_ms",
            SortKey::RecencyAt => "threads.recency_at_ms",
            SortKey::SectionPosition => "threads.section_position",
        };
        let operator = match sort_direction {
            SortDirection::Asc => ">",
            SortDirection::Desc => "<",
        };
        builder.push(" AND (");
        builder.push(column);
        builder.push(" ");
        builder.push(operator);
        builder.push(" ");
        builder.push_bind(anchor_ts);
        if include_thread_id_tiebreaker && let Some(anchor_id) = anchor.id {
            builder.push(" OR (");
            builder.push(column);
            builder.push(" = ");
            builder.push_bind(anchor_ts);
            builder.push(" AND threads.id ");
            builder.push(operator);
            builder.push(" ");
            builder.push_bind(anchor_id.to_string());
            builder.push(")");
        }
        builder.push(")");
    }
}

/// Controls whether SQLite may use the ordered column to satisfy `ORDER BY` from an index.
///
/// Disabling it adds a unary `+` to the ordered column. This preserves the sort semantics while
/// preventing a timestamp-only index from winning over a more selective filtering index.
#[derive(Clone, Copy)]
pub(super) enum OrderByIndex {
    Enabled,
    Disabled,
}

pub(super) fn push_thread_order_and_limit(
    builder: &mut QueryBuilder<Sqlite>,
    sort_key: SortKey,
    sort_direction: SortDirection,
    order_by_index: OrderByIndex,
    include_thread_id_tiebreaker: bool,
    limit: usize,
) {
    let order_column = match sort_key {
        SortKey::CreatedAt => "threads.created_at_ms",
        SortKey::UpdatedAt => "threads.updated_at_ms",
        SortKey::RecencyAt => "threads.recency_at_ms",
        SortKey::SectionPosition => "threads.section_position",
    };
    let order_direction = match sort_direction {
        SortDirection::Asc => "ASC",
        SortDirection::Desc => "DESC",
    };
    builder.push(" ORDER BY ");
    match order_by_index {
        OrderByIndex::Enabled => {}
        OrderByIndex::Disabled => {
            builder.push("+");
        }
    }
    builder.push(order_column);
    builder.push(" ");
    builder.push(order_direction);
    if include_thread_id_tiebreaker {
        builder.push(", threads.id ");
        builder.push(order_direction);
    }
    builder.push(" LIMIT ");
    builder.push_bind(limit as i64);
}
