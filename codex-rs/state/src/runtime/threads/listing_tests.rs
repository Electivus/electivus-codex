use super::*;
use crate::SortDirection;
use crate::SortKey;
use crate::ThreadSection;
use crate::runtime::test_support::test_thread_metadata;
use crate::runtime::test_support::unique_temp_dir;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;

const CUSTOM_THREAD_SECTION_ID: &str = "01984de2-8f74-7c91-a3b2-5c5e937cf317";

#[tokio::test]
async fn list_threads_filters_sections_before_recency_pagination_and_uses_index() {
    let codex_home = unique_temp_dir();
    let runtime = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("state db should initialize");
    let oldest_pinned = ThreadId::from_string("00000000-0000-0000-0000-000000000041").unwrap();
    let newest_unpinned = ThreadId::from_string("00000000-0000-0000-0000-000000000042").unwrap();
    let newest_pinned = ThreadId::from_string("00000000-0000-0000-0000-000000000043").unwrap();
    let oldest_unpinned = ThreadId::from_string("00000000-0000-0000-0000-000000000044").unwrap();

    for (thread_id, recency_at, section) in [
        (
            oldest_pinned,
            1_700_000_001,
            Some(crate::PINNED_THREAD_SECTION_ID),
        ),
        (newest_unpinned, 1_700_000_003, None),
        (
            newest_pinned,
            1_700_000_002,
            Some(crate::PINNED_THREAD_SECTION_ID),
        ),
        (oldest_unpinned, 1_700_000_000, None),
    ] {
        let mut metadata = test_thread_metadata(&codex_home, thread_id, codex_home.clone());
        metadata.recency_at = DateTime::<Utc>::from_timestamp(recency_at, 0).unwrap();
        metadata.section = section.map(|id| ThreadSection {
            id: id.to_string(),
            name: crate::PINNED_THREAD_SECTION_NAME.to_string(),
        });
        runtime.upsert_thread(&metadata).await.unwrap();
    }

    let filters = |anchor, section| ThreadFilterOptions {
        archived_only: false,
        allowed_sources: &[],
        model_providers: None,
        cwd_filters: None,
        repository_identity: None,
        section,
        anchor,
        sort_key: SortKey::RecencyAt,
        sort_direction: SortDirection::Desc,
        search_term: None,
    };
    let first_page = runtime
        .list_threads(
            /*page_size*/ 1,
            filters(None, Some(Some(crate::PINNED_THREAD_SECTION_ID))),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            first_page.items.len(),
            first_page.items[0].id,
            first_page.items[0].section.clone(),
        ),
        (
            1,
            newest_pinned,
            Some(ThreadSection {
                id: crate::PINNED_THREAD_SECTION_ID.to_string(),
                name: crate::PINNED_THREAD_SECTION_NAME.to_string(),
            }),
        )
    );
    let second_page = runtime
        .list_threads(
            /*page_size*/ 1,
            filters(
                first_page.next_anchor.as_ref(),
                Some(Some(crate::PINNED_THREAD_SECTION_ID)),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            second_page
                .items
                .iter()
                .map(|thread| thread.id)
                .collect::<Vec<_>>(),
            second_page.next_anchor,
        ),
        (vec![oldest_pinned], None)
    );

    let unsectioned_page = runtime
        .list_threads(/*page_size*/ 10, filters(None, Some(None)))
        .await
        .unwrap();
    assert_eq!(
        unsectioned_page
            .items
            .iter()
            .map(|thread| thread.id)
            .collect::<Vec<_>>(),
        vec![newest_unpinned, oldest_unpinned]
    );

    let all_sections_page = runtime
        .list_threads(/*page_size*/ 10, filters(None, None))
        .await
        .unwrap();
    assert_eq!(
        all_sections_page
            .items
            .iter()
            .map(|thread| thread.id)
            .collect::<Vec<_>>(),
        vec![
            newest_unpinned,
            newest_pinned,
            oldest_pinned,
            oldest_unpinned,
        ]
    );

    let mut builder = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
    push_list_threads_query(
        &mut builder,
        filters(None, Some(Some(crate::PINNED_THREAD_SECTION_ID))),
        /*relation_filter*/ None,
        /*limit*/ 2,
    );
    let plan_details = builder
        .build()
        .fetch_all(runtime.sqlite_pool().expect("SQLite runtime"))
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get::<String, _>("detail"))
        .collect::<Vec<_>>();
    assert!(
        plan_details
            .iter()
            .any(|detail| detail.contains("idx_threads_section_recency_at_ms")),
        "section listing did not use its selective recency index: {plan_details:?}"
    );
    assert!(
        !plan_details
            .iter()
            .any(|detail| detail.contains("TEMP B-TREE")),
        "section listing unexpectedly sorted outside its index: {plan_details:?}"
    );
}

#[tokio::test]
async fn section_position_listing_uses_stable_indexed_keyset_pagination() {
    let codex_home = unique_temp_dir();
    let runtime = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("state db should initialize");
    sqlx::query("INSERT INTO thread_sections (id, name) VALUES (?, ?)")
        .bind(CUSTOM_THREAD_SECTION_ID)
        .bind("Custom section")
        .execute(runtime.sqlite_pool().expect("SQLite runtime"))
        .await
        .expect("custom test section should be explicitly registered");
    let first = ThreadId::from_string("00000000-0000-0000-0000-000000000061").unwrap();
    let tied = ThreadId::from_string("00000000-0000-0000-0000-000000000062").unwrap();
    let last = ThreadId::from_string("00000000-0000-0000-0000-000000000063").unwrap();

    for (thread_id, position) in [(first, 1_000_000), (tied, 1_000_000), (last, 2_000_000)] {
        let mut metadata = test_thread_metadata(&codex_home, thread_id, codex_home.clone());
        metadata.section = Some(ThreadSection {
            id: CUSTOM_THREAD_SECTION_ID.to_string(),
            name: "Custom section".to_string(),
        });
        metadata.section_position = Some(position);
        metadata.section_entered_at = Some(metadata.updated_at);
        runtime.upsert_thread(&metadata).await.unwrap();
    }

    let filters = |anchor| ThreadFilterOptions {
        archived_only: false,
        allowed_sources: &[],
        model_providers: None,
        cwd_filters: None,
        repository_identity: None,
        section: Some(Some(CUSTOM_THREAD_SECTION_ID)),
        anchor,
        sort_key: SortKey::SectionPosition,
        sort_direction: SortDirection::Asc,
        search_term: None,
    };
    let first_page = runtime
        .list_threads(/*page_size*/ 1, filters(None))
        .await
        .unwrap();
    let second_page = runtime
        .list_threads(
            /*page_size*/ 1,
            filters(first_page.next_anchor.as_ref()),
        )
        .await
        .unwrap();
    let third_page = runtime
        .list_threads(
            /*page_size*/ 1,
            filters(second_page.next_anchor.as_ref()),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            [
                first_page.items[0].id,
                second_page.items[0].id,
                third_page.items[0].id,
            ],
            third_page.next_anchor,
        ),
        ([first, tied, last], None)
    );

    let mut builder = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
    push_list_threads_query(
        &mut builder,
        filters(/*anchor*/ None),
        /*relation_filter*/ None,
        /*limit*/ 2,
    );
    let plan_details = builder
        .build()
        .fetch_all(runtime.sqlite_pool().expect("SQLite runtime"))
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get::<String, _>("detail"))
        .collect::<Vec<_>>();
    assert!(
        plan_details
            .iter()
            .any(|detail| detail.contains("idx_threads_section_position")),
        "section-position listing did not use its selective index: {plan_details:?}"
    );
    assert!(
        !plan_details
            .iter()
            .any(|detail| detail.contains("TEMP B-TREE")),
        "section-position listing unexpectedly sorted outside its index: {plan_details:?}"
    );
}
