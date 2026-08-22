//! SQLite thread lookup, listing, relationship pagination, and cursor assembly.

use super::postgres;
use super::query::OrderByIndex;
use super::query::ThreadFilterOptions;
use super::query::push_thread_filters;
use super::query::push_thread_filters_with_preview;
use super::query::push_thread_order_and_limit;
use super::query::push_thread_select_columns;
use super::*;
use crate::SortDirection;

impl StateRuntime {
    /// Find a rollout path by thread id using the underlying database.
    pub async fn find_rollout_path_by_id(
        &self,
        id: ThreadId,
        archived_only: Option<bool>,
    ) -> anyhow::Result<Option<PathBuf>> {
        if let Some((pool, schema)) = self.postgres_connection() {
            return postgres::find_rollout_path_by_id(&pool, &schema, id, archived_only).await;
        }
        let mut builder =
            QueryBuilder::<Sqlite>::new("SELECT rollout_path FROM threads WHERE id = ");
        builder.push_bind(id.to_string());
        match archived_only {
            Some(true) => {
                builder.push(" AND archived = 1");
            }
            Some(false) => {
                builder.push(" AND archived = 0");
            }
            None => {}
        }
        let row = builder.build().fetch_optional(self.sqlite_pool()?).await?;
        Ok(row
            .and_then(|row| row.try_get::<String, _>("rollout_path").ok())
            .map(PathBuf::from))
    }

    /// Find the newest thread whose user-facing title exactly matches `title`.
    #[allow(clippy::too_many_arguments)]
    pub async fn find_thread_by_exact_title(
        &self,
        title: &str,
        allowed_sources: &[String],
        model_providers: Option<&[String]>,
        archived_only: bool,
        cwd: Option<&Path>,
    ) -> anyhow::Result<Option<crate::ThreadMetadata>> {
        let mut builder = QueryBuilder::<Sqlite>::new("");
        push_thread_select_columns(&mut builder);
        builder.push(" FROM threads");
        push_thread_filters(
            &mut builder,
            ThreadFilterOptions {
                archived_only,
                allowed_sources,
                model_providers,
                cwd_filters: None,
                repository_identity: None,
                section: None,
                project_id: None,
                anchor: None,
                sort_key: crate::SortKey::UpdatedAt,
                sort_direction: SortDirection::Desc,
                search_term: None,
            },
            /*include_thread_id_tiebreaker*/ false,
        );
        builder.push(" AND threads.title = ");
        builder.push_bind(title);
        if let Some(cwd) = cwd {
            builder.push(" AND threads.cwd = ");
            builder.push_bind(cwd.display().to_string());
        }
        push_thread_order_and_limit(
            &mut builder,
            crate::SortKey::UpdatedAt,
            SortDirection::Desc,
            OrderByIndex::Enabled,
            /*include_thread_id_tiebreaker*/ false,
            /*limit*/ 1,
        );

        let row = builder.build().fetch_optional(self.sqlite_pool()?).await?;
        row.map(|row| ThreadRow::try_from_row(&row).and_then(crate::ThreadMetadata::try_from))
            .transpose()
    }

    /// List threads using the underlying database.
    pub async fn list_threads(
        &self,
        page_size: usize,
        filters: ThreadFilterOptions<'_>,
    ) -> anyhow::Result<crate::ThreadsPage> {
        self.list_threads_matching(page_size, filters, /*relation_filter*/ None)
            .await
    }

    /// List direct children of `parent_thread_id` using persisted spawn edges.
    pub async fn list_threads_by_parent(
        &self,
        page_size: usize,
        parent_thread_id: ThreadId,
        filters: ThreadFilterOptions<'_>,
    ) -> anyhow::Result<crate::ThreadsPage> {
        self.list_threads_by_relation(
            page_size,
            crate::ThreadRelationFilter::DirectChildrenOf(parent_thread_id),
            filters,
        )
        .await
    }

    /// List threads matching a persisted spawn-graph relationship.
    pub async fn list_threads_by_relation(
        &self,
        page_size: usize,
        relation_filter: crate::ThreadRelationFilter,
        filters: ThreadFilterOptions<'_>,
    ) -> anyhow::Result<crate::ThreadsPage> {
        self.list_threads_matching(page_size, filters, Some(relation_filter))
            .await
    }

    async fn list_threads_matching(
        &self,
        page_size: usize,
        filters: ThreadFilterOptions<'_>,
        relation_filter: Option<crate::ThreadRelationFilter>,
    ) -> anyhow::Result<crate::ThreadsPage> {
        let limit = page_size.saturating_add(1);
        let include_thread_id_tiebreaker =
            should_include_thread_id_tiebreaker(filters, relation_filter);

        let mut builder = QueryBuilder::<Sqlite>::new("");
        push_list_threads_query(&mut builder, filters, relation_filter, limit);

        let rows = builder.build().fetch_all(self.sqlite_pool()?).await?;
        let mut items = Vec::with_capacity(rows.len());
        let mut parent_thread_ids = std::collections::HashMap::new();
        for row in rows {
            let item = ThreadRow::try_from_row(&row).and_then(ThreadMetadata::try_from)?;
            if relation_filter.is_some()
                && let Some(parent_thread_id) =
                    row.try_get::<Option<String>, _>("parent_thread_id")?
            {
                parent_thread_ids.insert(item.id, ThreadId::try_from(parent_thread_id)?);
            }
            items.push(item);
        }
        let num_scanned_rows = items.len();
        let next_anchor = if items.len() > page_size {
            if let Some(overflow_item) = items.pop() {
                parent_thread_ids.remove(&overflow_item.id);
            }
            items.last().and_then(|item| {
                anchor_from_item(item, filters.sort_key, include_thread_id_tiebreaker)
            })
        } else {
            None
        };
        Ok(ThreadsPage {
            items,
            parent_thread_ids,
            next_anchor,
            num_scanned_rows,
        })
    }

    /// List thread ids using the underlying database (no rollout scanning).
    pub async fn list_thread_ids(
        &self,
        limit: usize,
        anchor: Option<&crate::Anchor>,
        sort_key: crate::SortKey,
        allowed_sources: &[String],
        model_providers: Option<&[String]>,
        archived_only: bool,
    ) -> anyhow::Result<Vec<ThreadId>> {
        let mut builder = QueryBuilder::<Sqlite>::new("SELECT threads.id FROM threads");
        push_thread_filters(
            &mut builder,
            ThreadFilterOptions {
                archived_only,
                allowed_sources,
                model_providers,
                cwd_filters: None,
                repository_identity: None,
                section: None,
                project_id: None,
                anchor,
                sort_key,
                sort_direction: SortDirection::Desc,
                search_term: None,
            },
            matches!(
                sort_key,
                crate::SortKey::RecencyAt | crate::SortKey::SectionPosition
            ),
        );
        push_thread_order_and_limit(
            &mut builder,
            sort_key,
            SortDirection::Desc,
            OrderByIndex::Enabled,
            matches!(
                sort_key,
                crate::SortKey::RecencyAt | crate::SortKey::SectionPosition
            ),
            limit,
        );

        let rows = builder.build().fetch_all(self.sqlite_pool()?).await?;
        rows.into_iter()
            .map(|row| {
                let id: String = row.try_get("id")?;
                Ok(ThreadId::try_from(id)?)
            })
            .collect()
    }
}

pub(super) fn push_list_threads_query(
    builder: &mut QueryBuilder<Sqlite>,
    filters: ThreadFilterOptions<'_>,
    relation_filter: Option<crate::ThreadRelationFilter>,
    limit: usize,
) {
    if let Some(crate::ThreadRelationFilter::DescendantsOf(ancestor_thread_id)) = relation_filter {
        builder.push(
            r#"
WITH RECURSIVE subtree(child_thread_id, parent_thread_id) AS (
    SELECT child_thread_id, parent_thread_id
    FROM thread_spawn_edges
    WHERE parent_thread_id =
"#,
        );
        builder.push_bind(ancestor_thread_id.to_string());
        builder.push(
            r#"
    UNION
    SELECT edge.child_thread_id, edge.parent_thread_id
    FROM thread_spawn_edges AS edge
    JOIN subtree ON edge.parent_thread_id = subtree.child_thread_id
)
"#,
        );
    }
    push_thread_select_columns(builder);
    // SQLite may otherwise reorder these joins and scan the global timestamp index before
    // checking the relationship. CROSS JOIN keeps the selective edge/subtree traversal first.
    match relation_filter {
        Some(crate::ThreadRelationFilter::DirectChildrenOf(_)) => builder.push(
            ", listed_edge.parent_thread_id AS parent_thread_id\nFROM thread_spawn_edges AS listed_edge\nCROSS JOIN threads ON threads.id = listed_edge.child_thread_id",
        ),
        Some(crate::ThreadRelationFilter::DescendantsOf(_)) => builder.push(
            ", subtree.parent_thread_id AS parent_thread_id\nFROM subtree\nCROSS JOIN threads ON threads.id = subtree.child_thread_id",
        ),
        None => builder.push(" FROM threads"),
    };
    let include_thread_id_tiebreaker =
        should_include_thread_id_tiebreaker(filters, relation_filter);
    let include_empty_preview =
        relation_filter.is_some() || matches!(filters.section, Some(Some(_)));
    push_thread_filters_with_preview(
        builder,
        filters,
        include_thread_id_tiebreaker,
        include_empty_preview,
    );
    match relation_filter {
        Some(crate::ThreadRelationFilter::DirectChildrenOf(parent_thread_id)) => {
            builder.push(" AND listed_edge.parent_thread_id = ");
            builder.push_bind(parent_thread_id.to_string());
        }
        Some(crate::ThreadRelationFilter::DescendantsOf(ancestor_thread_id)) => {
            builder.push(" AND subtree.child_thread_id != ");
            builder.push_bind(ancestor_thread_id.to_string());
        }
        None => {}
    }
    let order_by_index = match (
        relation_filter,
        filters.cwd_filters,
        filters.repository_identity,
    ) {
        // Relationship listings are expected to be much smaller than the global thread table.
        // Prefer the spawn-edge index and sort the matching subtree instead of scanning the
        // timestamp index until enough related threads happen to be found.
        (Some(_), _, _) => OrderByIndex::Disabled,
        // Project Session Scope uses an OR across cwd and repository identity. Prefer the
        // selective location indexes and sort the matched set.
        (None, _, Some(_)) => OrderByIndex::Disabled,
        // Multi-cwd listing is supported but at the time of writing has no current use in
        // production. Preserve its query plan so the global timestamp index does not regress cwd
        // filtering into a scan.
        (None, Some(cwd_filters), None) if cwd_filters.len() > 1 => OrderByIndex::Disabled,
        (None, Some(_) | None, None) => OrderByIndex::Enabled,
    };
    push_thread_order_and_limit(
        builder,
        filters.sort_key,
        filters.sort_direction,
        order_by_index,
        include_thread_id_tiebreaker,
        limit,
    );
}

fn should_include_thread_id_tiebreaker(
    filters: ThreadFilterOptions<'_>,
    relation_filter: Option<crate::ThreadRelationFilter>,
) -> bool {
    relation_filter.is_some()
        || matches!(
            filters.sort_key,
            SortKey::RecencyAt | SortKey::SectionPosition
        )
        || filters.repository_identity.is_some()
}

#[cfg(test)]
#[path = "listing_tests.rs"]
mod tests;
