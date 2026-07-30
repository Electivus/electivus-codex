use std::path::Path;

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::CreateThreadParams;
use crate::ListThreadsParams;
use crate::LocalThreadStore;
use crate::LocalThreadStoreConfig;
use crate::PostgresThreadStore;
use crate::SortDirection;
use crate::ThreadLocationFilter;
use crate::ThreadMetadataPatch;
use crate::ThreadPersistenceMetadata;
use crate::ThreadRelationFilter;
use crate::ThreadSortKey;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::UpdateThreadMetadataParams;
use crate::postgres_contract_tests::PostgresThreadStoreFixture;

#[tokio::test]
async fn local_list_threads_matches_public_store_contract() -> Result<(), Box<dyn std::error::Error>>
{
    let home = TempDir::new()?;
    let config = LocalThreadStoreConfig {
        codex_home: home.path().to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        default_model_provider_id: "list-contract-provider".to_string(),
    };
    let runtime = codex_state::StateRuntime::init_sqlite(
        home.path().to_path_buf(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await?;
    let store = LocalThreadStore::new(config, Some(runtime));

    assert_list_threads_contract(&store, home.path()).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_list_threads_matches_public_store_contract()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("list_threads_order")?;
    fixture.migrate().await?;
    let pool = fixture.connect_pool().await?;
    let reader_pool = fixture.connect_pool().await?;
    let store = PostgresThreadStore::new(pool.clone(), fixture.schema.clone());
    let reader = PostgresThreadStore::new(reader_pool.clone(), fixture.schema.clone());

    assert_list_threads_contract(&store, Path::new("/list-contract")).await?;
    let parent_thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f301")?;
    update_listing_metadata(
        &store,
        parent_thread_id,
        "ordinary preview",
        Some("needle explicit name"),
        "provider-a",
        SessionSource::Cli,
        Path::new("/list-contract/a"),
    )
    .await?;
    let mut name_search = list_params(
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
    );
    name_search.search_term = Some("needle explicit".to_string());
    name_search.model_providers = None;
    name_search.use_state_db_only = false;
    assert_eq!(
        thread_ids_from_page(&store.list_threads(name_search).await?),
        vec![parent_thread_id]
    );
    assert_relation_filters(
        &store,
        &reader,
        Path::new("/list-contract"),
        parent_thread_id,
    )
    .await?;
    pool.close().await;
    reader_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_project_list_keeps_tied_cursor_stable_across_replicas()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("list_threads_replicas")?;
    fixture.migrate().await?;
    let writer_pool = fixture.connect_pool().await?;
    let reader_pool = fixture.connect_pool().await?;
    let writer = PostgresThreadStore::new(writer_pool.clone(), fixture.schema.clone());
    let reader = PostgresThreadStore::new(reader_pool.clone(), fixture.schema.clone());
    let project_filter = ThreadLocationFilter::ProjectSessionScope {
        cwd: Path::new("/list-replica").to_path_buf(),
        repository_identity: "example.com/acme/replica".to_string(),
    };
    let lower_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f311")?;
    let higher_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f312")?;
    for thread_id in [lower_id, higher_id] {
        create_listed_thread(
            &writer,
            ListedThread {
                thread_id,
                cwd: Path::new("/list-replica"),
                timestamp: "2040-01-01T00:00:00Z",
                source: SessionSource::Cli,
                model_provider: "replica-provider",
                parent_thread_id: None,
                preview: "replica preview",
                name: None,
                history_mode: ThreadHistoryMode::Legacy,
                items: Vec::new(),
            },
        )
        .await?;
    }
    let mut cursors = Vec::new();
    for sort_key in [
        ThreadSortKey::CreatedAt,
        ThreadSortKey::UpdatedAt,
        ThreadSortKey::RecencyAt,
    ] {
        let mut params = list_params(
            /*page_size*/ 1,
            /*cursor*/ None,
            sort_key,
            SortDirection::Desc,
        );
        params.location_filter = project_filter.clone();
        let first = reader.list_threads(params).await?;
        assert_eq!(thread_ids_from_page(&first), vec![higher_id]);
        cursors.push((
            sort_key,
            first.next_cursor.expect("first page should have a cursor"),
        ));
    }
    let newer_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f313")?;
    create_listed_thread(
        &writer,
        ListedThread {
            thread_id: newer_id,
            cwd: Path::new("/list-replica-unrelated"),
            timestamp: "2040-01-02T00:00:00Z",
            source: SessionSource::Cli,
            model_provider: "replica-provider",
            parent_thread_id: None,
            preview: "replica preview",
            name: None,
            history_mode: ThreadHistoryMode::Legacy,
            items: Vec::new(),
        },
    )
    .await?;
    for (sort_key, cursor) in cursors {
        let mut params = list_params(
            /*page_size*/ 1,
            Some(cursor),
            sort_key,
            SortDirection::Desc,
        );
        params.location_filter = project_filter.clone();
        let second = reader.list_threads(params).await?;
        assert_eq!(thread_ids_from_page(&second), vec![lower_id]);
        assert_eq!(second.next_cursor, None);
    }
    writer_pool.close().await;
    reader_pool.close().await;
    fixture.cleanup().await
}

async fn assert_list_threads_contract(
    store: &dyn ThreadStore,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let thread_ids = [
        ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f301")?,
        ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f302")?,
        ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f303")?,
    ];
    for (thread_id, timestamp) in thread_ids.iter().zip([
        "2030-01-01T00:00:00Z",
        "2030-01-02T00:00:00Z",
        "2030-01-03T00:00:00Z",
    ]) {
        create_listed_thread(
            store,
            ListedThread {
                thread_id: *thread_id,
                cwd,
                timestamp,
                source: SessionSource::Exec,
                model_provider: "list-contract-provider",
                parent_thread_id: None,
                preview: &format!("visible {thread_id}"),
                name: None,
                history_mode: ThreadHistoryMode::Legacy,
                items: Vec::new(),
            },
        )
        .await?;
    }

    let first = store
        .list_threads(list_params(
            /*page_size*/ 2,
            /*cursor*/ None,
            ThreadSortKey::CreatedAt,
            SortDirection::Desc,
        ))
        .await?;
    assert_eq!(
        thread_ids_from_page(&first),
        vec![thread_ids[2], thread_ids[1]]
    );
    let second = store
        .list_threads(list_params(
            /*page_size*/ 2,
            first.next_cursor,
            ThreadSortKey::CreatedAt,
            SortDirection::Desc,
        ))
        .await?;
    assert_eq!(thread_ids_from_page(&second), vec![thread_ids[0]]);
    assert_eq!(second.next_cursor, None);

    for (sort_key, sort_direction, expected) in [
        (
            ThreadSortKey::CreatedAt,
            SortDirection::Asc,
            thread_ids.to_vec(),
        ),
        (
            ThreadSortKey::UpdatedAt,
            SortDirection::Desc,
            thread_ids.into_iter().rev().collect(),
        ),
        (
            ThreadSortKey::UpdatedAt,
            SortDirection::Asc,
            thread_ids.to_vec(),
        ),
        (
            ThreadSortKey::RecencyAt,
            SortDirection::Asc,
            thread_ids.to_vec(),
        ),
    ] {
        let page = store
            .list_threads(list_params(
                /*page_size*/ 10,
                /*cursor*/ None,
                sort_key,
                sort_direction,
            ))
            .await?;
        assert_eq!(thread_ids_from_page(&page), expected);
        assert_eq!(page.next_cursor, None);
    }

    update_listing_metadata(
        store,
        thread_ids[0],
        "needle name",
        /*name*/ None,
        "provider-a",
        SessionSource::Cli,
        &cwd.join("a"),
    )
    .await?;
    update_listing_metadata(
        store,
        thread_ids[1],
        "needle preview",
        /*name*/ None,
        "provider-a",
        SessionSource::Cli,
        &cwd.join("a"),
    )
    .await?;
    update_listing_metadata(
        store,
        thread_ids[2],
        "filtered out",
        /*name*/ None,
        "provider-b",
        SessionSource::Exec,
        &cwd.join("b"),
    )
    .await?;
    let mut combined = list_params(
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
    );
    combined.allowed_sources = vec![SessionSource::Cli];
    combined.model_providers = Some(vec!["provider-a".to_string()]);
    combined.location_filter = ThreadLocationFilter::ExactCwds(vec![cwd.join("a")]);
    combined.search_term = Some("needle".to_string());
    assert_eq!(
        thread_ids_from_page(&store.list_threads(combined.clone()).await?),
        vec![thread_ids[1], thread_ids[0]]
    );
    combined.location_filter = ThreadLocationFilter::ExactCwds(Vec::new());
    assert!(store.list_threads(combined).await?.items.is_empty());

    let pinned = store
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id: thread_ids[0],
            patch: ThreadMetadataPatch {
                is_pinned: Some(true),
                ..Default::default()
            },
            include_archived: false,
        })
        .await?;
    assert!(pinned.is_pinned);
    let mut pin_filter = list_params(
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
    );
    pin_filter.is_pinned = Some(true);
    assert_eq!(
        thread_ids_from_page(&store.list_threads(pin_filter.clone()).await?),
        vec![thread_ids[0]]
    );
    pin_filter.is_pinned = Some(false);
    assert_eq!(
        thread_ids_from_page(&store.list_threads(pin_filter).await?),
        vec![thread_ids[2], thread_ids[1]]
    );

    store
        .archive_thread(ArchiveThreadParams {
            thread_id: thread_ids[2],
        })
        .await?;
    let mut archived = list_params(
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
    );
    archived.archived = true;
    archived.model_providers = Some(vec!["provider-b".to_string()]);
    assert_eq!(
        thread_ids_from_page(&store.list_threads(archived).await?),
        vec![thread_ids[2]]
    );
    let mut unmatched = list_params(
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
    );
    unmatched.allowed_sources = vec![SessionSource::Exec];
    assert_empty_page(store, unmatched.clone()).await?;
    unmatched.allowed_sources.clear();
    unmatched.model_providers = Some(vec!["missing-provider".to_string()]);
    assert_empty_page(store, unmatched.clone()).await?;
    unmatched.model_providers = Some(Vec::new());
    unmatched.location_filter = ThreadLocationFilter::ExactCwds(vec![cwd.join("b")]);
    assert_empty_page(store, unmatched.clone()).await?;
    unmatched.location_filter = ThreadLocationFilter::Unrestricted;
    unmatched.search_term = Some("filtered out".to_string());
    assert_empty_page(store, unmatched).await?;

    let legacy_cursor = store
        .list_threads(list_params(
            /*page_size*/ 10,
            Some("2030-01-02T00-00-00".to_string()),
            ThreadSortKey::CreatedAt,
            SortDirection::Desc,
        ))
        .await?;
    assert_eq!(thread_ids_from_page(&legacy_cursor), vec![thread_ids[0]]);

    let empty_page = store
        .list_threads(list_params(
            /*page_size*/ 0,
            /*cursor*/ None,
            ThreadSortKey::CreatedAt,
            SortDirection::Desc,
        ))
        .await?;
    assert_eq!(thread_ids_from_page(&empty_page), Vec::new());
    assert_eq!(empty_page.next_cursor, None);

    let invalid_cursor = store
        .list_threads(ListThreadsParams {
            cursor: Some("not-a-cursor".to_string()),
            ..list_params(
                /*page_size*/ 10,
                /*cursor*/ None,
                ThreadSortKey::CreatedAt,
                SortDirection::Desc,
            )
        })
        .await;
    assert!(matches!(
        invalid_cursor,
        Err(ThreadStoreError::InvalidRequest { .. })
    ));

    assert_project_session_scope_contract(store, cwd).await?;
    Ok(())
}

async fn assert_project_session_scope_contract(
    store: &dyn ThreadStore,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let exact_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f401")?;
    let same_repository_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f402")?;
    let unrelated_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f403")?;
    let null_identity_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f404")?;
    let archived_same_repository_id =
        ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f405")?;
    let project_cwd = cwd.join("project");
    let other_cwd = cwd.join("other-checkout");
    for (thread_id, thread_cwd, timestamp, source, parent_thread_id, preview) in [
        (
            exact_id,
            project_cwd.as_path(),
            "2040-01-01T00:00:00Z",
            SessionSource::Cli,
            None,
            "project needle exact",
        ),
        (
            same_repository_id,
            other_cwd.as_path(),
            "2040-01-02T00:00:00Z",
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: exact_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            Some(exact_id),
            "project needle related",
        ),
        (
            unrelated_id,
            other_cwd.as_path(),
            "2040-01-03T00:00:00Z",
            SessionSource::Cli,
            None,
            "project needle unrelated",
        ),
        (
            null_identity_id,
            other_cwd.as_path(),
            "2040-01-04T00:00:00Z",
            SessionSource::Cli,
            None,
            "project needle null",
        ),
        (
            archived_same_repository_id,
            other_cwd.as_path(),
            "2040-01-05T00:00:00Z",
            SessionSource::Cli,
            None,
            "project needle archived",
        ),
    ] {
        create_listed_thread(
            store,
            ListedThread {
                thread_id,
                cwd: thread_cwd,
                timestamp,
                source,
                model_provider: "project-provider",
                parent_thread_id,
                preview,
                name: None,
                history_mode: ThreadHistoryMode::Legacy,
                items: Vec::new(),
            },
        )
        .await?;
    }
    for (thread_id, origin_url) in [
        (same_repository_id, "ssh://git@example.com/acme/project.git"),
        (
            archived_same_repository_id,
            "https://example.com/acme/project.git",
        ),
        (unrelated_id, "https://example.com/acme/unrelated.git"),
    ] {
        set_repository_origin(store, thread_id, origin_url).await?;
    }
    for (thread_id, timestamp) in [
        (exact_id, "2040-01-01T00:00:00Z"),
        (same_repository_id, "2040-01-01T00:00:00Z"),
    ] {
        let timestamp = DateTime::parse_from_rfc3339(timestamp)?.with_timezone(&Utc);
        store
            .update_thread_metadata(UpdateThreadMetadataParams {
                thread_id,
                patch: ThreadMetadataPatch {
                    created_at: Some(timestamp),
                    updated_at: Some(timestamp),
                    advance_recency_at: Some(timestamp),
                    ..Default::default()
                },
                include_archived: false,
            })
            .await?;
    }
    store
        .archive_thread(ArchiveThreadParams {
            thread_id: archived_same_repository_id,
        })
        .await?;
    assert_eq!(
        store
            .read_thread(crate::ReadThreadParams {
                thread_id: exact_id,
                include_archived: false,
                include_history: false,
            })
            .await?
            .cwd,
        project_cwd
    );

    let project_filter = ThreadLocationFilter::ProjectSessionScope {
        cwd: project_cwd.clone(),
        repository_identity: "example.com/acme/project".to_string(),
    };
    for (sort_key, sort_direction, expected) in [
        (
            ThreadSortKey::CreatedAt,
            SortDirection::Asc,
            vec![exact_id, same_repository_id],
        ),
        (
            ThreadSortKey::CreatedAt,
            SortDirection::Desc,
            vec![same_repository_id, exact_id],
        ),
        (
            ThreadSortKey::UpdatedAt,
            SortDirection::Asc,
            vec![exact_id, same_repository_id],
        ),
        (
            ThreadSortKey::UpdatedAt,
            SortDirection::Desc,
            vec![same_repository_id, exact_id],
        ),
        (
            ThreadSortKey::RecencyAt,
            SortDirection::Asc,
            vec![exact_id, same_repository_id],
        ),
        (
            ThreadSortKey::RecencyAt,
            SortDirection::Desc,
            vec![same_repository_id, exact_id],
        ),
    ] {
        let mut first_params =
            project_list_params(sort_key, sort_direction, project_filter.clone());
        first_params.page_size = 1;
        let first = store.list_threads(first_params).await?;
        let mut second_params =
            project_list_params(sort_key, sort_direction, project_filter.clone());
        second_params.page_size = 1;
        second_params.cursor = first.next_cursor.clone();
        let second = store.list_threads(second_params).await?;
        assert_eq!(
            [thread_ids_from_page(&first), thread_ids_from_page(&second)].concat(),
            expected
        );
    }

    let mut exact = project_list_params(
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        ThreadLocationFilter::ExactCwds(vec![project_cwd]),
    );
    assert_eq!(
        thread_ids_from_page(&store.list_threads(exact.clone()).await?),
        vec![exact_id]
    );
    exact.location_filter = project_filter.clone();
    exact.search_term = Some("needle".to_string());
    assert_eq!(
        thread_ids_from_page(&store.list_threads(exact.clone()).await?),
        vec![same_repository_id, exact_id]
    );
    exact.allowed_sources = vec![SessionSource::Cli];
    assert_eq!(
        thread_ids_from_page(&store.list_threads(exact.clone()).await?),
        vec![exact_id]
    );

    store
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id: exact_id,
            patch: ThreadMetadataPatch {
                is_pinned: Some(true),
                ..Default::default()
            },
            include_archived: false,
        })
        .await?;
    exact.allowed_sources.clear();
    exact.is_pinned = Some(true);
    assert_eq!(
        thread_ids_from_page(&store.list_threads(exact.clone()).await?),
        vec![exact_id]
    );
    exact.is_pinned = None;
    exact.relation_filter = Some(ThreadRelationFilter::DirectChildrenOf(exact_id));
    assert_eq!(
        thread_ids_from_page(&store.list_threads(exact.clone()).await?),
        vec![same_repository_id]
    );
    exact.relation_filter = None;
    exact.archived = true;
    assert_eq!(
        thread_ids_from_page(&store.list_threads(exact).await?),
        vec![archived_same_repository_id]
    );
    Ok(())
}

fn project_list_params(
    sort_key: ThreadSortKey,
    sort_direction: SortDirection,
    location_filter: ThreadLocationFilter,
) -> ListThreadsParams {
    let mut params = list_params(
        /*page_size*/ 10,
        /*cursor*/ None,
        sort_key,
        sort_direction,
    );
    params.location_filter = location_filter;
    params.model_providers = Some(vec!["project-provider".to_string()]);
    params
}

async fn assert_relation_filters(
    writer: &dyn ThreadStore,
    reader: &dyn ThreadStore,
    cwd: &Path,
    parent_thread_id: ThreadId,
) -> Result<(), Box<dyn std::error::Error>> {
    let child_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f304")?;
    let grandchild_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f305")?;
    let archived_child_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f306")?;
    for (thread_id, parent_thread_id, depth, timestamp) in [
        (child_id, parent_thread_id, 1, "2030-01-04T00:00:00Z"),
        (grandchild_id, child_id, 2, "2030-01-05T00:00:00Z"),
        (
            archived_child_id,
            parent_thread_id,
            1,
            "2030-01-06T00:00:00Z",
        ),
    ] {
        create_listed_thread(
            writer,
            ListedThread {
                thread_id,
                cwd,
                timestamp,
                source: SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                }),
                model_provider: "relation-provider",
                parent_thread_id: Some(parent_thread_id),
                preview: "relation preview",
                name: None,
                history_mode: ThreadHistoryMode::Legacy,
                items: Vec::new(),
            },
        )
        .await?;
    }
    writer
        .archive_thread(ArchiveThreadParams {
            thread_id: archived_child_id,
        })
        .await?;

    let mut related = list_params(
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
    );
    related.model_providers = Some(vec!["relation-provider".to_string()]);
    related.search_term = Some("relation".to_string());
    related.relation_filter = Some(ThreadRelationFilter::DirectChildrenOf(parent_thread_id));
    assert_eq!(
        thread_ids_from_page(&reader.list_threads(related.clone()).await?),
        vec![child_id]
    );
    related.relation_filter = Some(ThreadRelationFilter::DescendantsOf(parent_thread_id));
    assert_eq!(
        thread_ids_from_page(&reader.list_threads(related.clone()).await?),
        vec![grandchild_id, child_id]
    );
    related.archived = true;
    related.relation_filter = Some(ThreadRelationFilter::DirectChildrenOf(parent_thread_id));
    assert_eq!(
        thread_ids_from_page(&reader.list_threads(related).await?),
        vec![archived_child_id]
    );
    Ok(())
}

pub(super) struct ListedThread<'a> {
    pub(super) thread_id: ThreadId,
    pub(super) cwd: &'a Path,
    pub(super) timestamp: &'a str,
    pub(super) source: SessionSource,
    pub(super) model_provider: &'a str,
    pub(super) parent_thread_id: Option<ThreadId>,
    pub(super) preview: &'a str,
    pub(super) name: Option<&'a str>,
    pub(super) history_mode: ThreadHistoryMode,
    pub(super) items: Vec<RolloutItem>,
}

pub(super) async fn create_listed_thread(
    store: &dyn ThreadStore,
    thread: ListedThread<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ListedThread {
        thread_id,
        cwd,
        timestamp,
        source,
        model_provider,
        parent_thread_id,
        preview,
        name,
        history_mode,
        items,
    } = thread;
    store
        .create_thread(CreateThreadParams {
            session_id: thread_id.into(),
            thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id,
            source: source.clone(),
            thread_source: None,
            originator: "list-threads-contract".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode,
            history_base: None,
            subagent_history_start_ordinal: None,
            initial_window_id: "list-threads-window".to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(cwd.to_path_buf()),
                model_provider: model_provider.to_string(),
                memory_mode: ThreadMemoryMode::Enabled,
            },
        })
        .await?;
    if !items.is_empty() {
        store
            .append_items(AppendThreadItemsParams { thread_id, items })
            .await?;
    }
    store.persist_thread(thread_id).await?;
    store.shutdown_thread(thread_id).await?;
    let timestamp = DateTime::parse_from_rfc3339(timestamp)?.with_timezone(&Utc);
    store
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch: ThreadMetadataPatch {
                preview: Some(preview.to_string()),
                first_user_message: Some(preview.to_string()),
                name: name.map(str::to_string).map(Some),
                created_at: Some(timestamp),
                updated_at: Some(timestamp),
                advance_recency_at: Some(timestamp),
                source: Some(source),
                cwd: Some(cwd.to_path_buf()),
                model_provider: Some(model_provider.to_string()),
                ..Default::default()
            },
            include_archived: false,
        })
        .await?;
    Ok(())
}

async fn update_listing_metadata(
    store: &dyn ThreadStore,
    thread_id: ThreadId,
    preview: &str,
    name: Option<&str>,
    model_provider: &str,
    source: SessionSource,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    store
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch: ThreadMetadataPatch {
                preview: Some(preview.to_string()),
                first_user_message: Some(preview.to_string()),
                name: name.map(str::to_string).map(Some),
                model_provider: Some(model_provider.to_string()),
                source: Some(source),
                cwd: Some(cwd.to_path_buf()),
                ..Default::default()
            },
            include_archived: false,
        })
        .await?;
    Ok(())
}

async fn set_repository_origin(
    store: &dyn ThreadStore,
    thread_id: ThreadId,
    origin_url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    store
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch: ThreadMetadataPatch {
                git_info: Some(crate::GitInfoPatch {
                    origin_url: Some(Some(origin_url.to_string())),
                    ..Default::default()
                }),
                ..Default::default()
            },
            include_archived: false,
        })
        .await?;
    Ok(())
}

fn list_params(
    page_size: usize,
    cursor: Option<String>,
    sort_key: ThreadSortKey,
    sort_direction: SortDirection,
) -> ListThreadsParams {
    ListThreadsParams {
        page_size,
        cursor,
        sort_key,
        sort_direction,
        allowed_sources: Vec::new(),
        model_providers: Some(Vec::new()),
        location_filter: crate::ThreadLocationFilter::Unrestricted,
        is_pinned: None,
        archived: false,
        search_term: None,
        relation_filter: None,
        use_state_db_only: true,
    }
}

fn thread_ids_from_page(page: &crate::ThreadPage) -> Vec<ThreadId> {
    page.items.iter().map(|thread| thread.thread_id).collect()
}

async fn assert_empty_page(
    store: &dyn ThreadStore,
    params: ListThreadsParams,
) -> Result<(), Box<dyn std::error::Error>> {
    assert!(store.list_threads(params).await?.items.is_empty());
    Ok(())
}
