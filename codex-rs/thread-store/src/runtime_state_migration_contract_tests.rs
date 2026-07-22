use crate::ListItemsParams;
use crate::ListThreadsParams;
use crate::ListTurnsParams;
use crate::PostgresThreadStore;
use crate::ReadThreadParams;
use crate::SearchThreadsParams;
use crate::SortDirection;
use crate::StoredTurnItemsView;
use crate::ThreadRelationFilter;
use crate::ThreadSortKey;
use crate::ThreadStore;
use crate::postgres_contract_tests::PostgresThreadStoreFixture;
use crate::postgres_turn_projection_contract_tests::completed_item;
use crate::postgres_turn_projection_contract_tests::turn_complete;
use crate::postgres_turn_projection_contract_tests::turn_started;
use chrono::TimeZone;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::user_input::UserInput;
use codex_state::BackfillState;
use codex_state::BackfillStatus;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use codex_state::PostgresRuntimeStatePool;
use codex_state::RuntimeStateMigrationPhase;
use codex_state::SqliteConfig;
use codex_state::ThreadMetadataBuilder;
use codex_state::import_runtime_state_threads;
use codex_state::open_thread_history_db;
use codex_state::preflight_runtime_state_migration;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;

pub(super) enum LineageFixture {
    Valid,
    Orphan,
}

pub(super) struct MigrationSource {
    _temp: tempfile::TempDir,
    pub(super) config: SqliteConfig,
    thread_id: ThreadId,
    history: Vec<RolloutLine>,
    legacy_id: ThreadId,
    legacy_history: Vec<RolloutLine>,
    rollout_path: std::path::PathBuf,
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn runtime_state_thread_import_is_visible_to_another_pool()
-> Result<(), Box<dyn std::error::Error>> {
    let source = migration_source(LineageFixture::Valid).await?;
    let fixture = PostgresThreadStoreFixture::new("runtime_migration")?;
    fixture.migrate().await?;
    let inventory =
        preflight_runtime_state_migration(source.config.clone(), fixture.config.clone()).await?;
    let progress = import_runtime_state_threads(
        &source.config,
        &fixture.config,
        &inventory,
        &codex_rollout::CanonicalRolloutHistoryReader,
    )
    .await?;
    assert_eq!(
        progress.phase(),
        RuntimeStateMigrationPhase::ThreadsImported
    );
    assert_eq!(
        import_runtime_state_threads(
            &source.config,
            &fixture.config,
            &inventory,
            &codex_rollout::CanonicalRolloutHistoryReader,
        )
        .await?,
        progress
    );

    let replica_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let second_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let replica = PostgresThreadStore::new(&replica_pool);
    let second = PostgresThreadStore::new(&second_pool);
    let imported = replica
        .read_thread(ReadThreadParams {
            thread_id: source.thread_id,
            include_archived: true,
            include_history: true,
        })
        .await?;
    let from_second = second
        .read_thread(ReadThreadParams {
            thread_id: source.thread_id,
            include_archived: true,
            include_history: true,
        })
        .await?;
    assert_eq!(
        serde_json::to_value(&from_second)?,
        serde_json::to_value(&imported)?
    );
    assert_eq!(imported.name.as_deref(), Some("Migrated thread"));
    assert_eq!(imported.preview, "migration preview");
    assert_eq!(
        imported.token_usage,
        Some(TokenUsage {
            total_tokens: 987,
            ..Default::default()
        })
    );
    assert_eq!(
        serde_json::to_value(imported.history.ok_or("history")?.items)?,
        serde_json::to_value(
            source
                .history
                .into_iter()
                .map(|line| line.item)
                .collect::<Vec<_>>()
        )?
    );
    let listed = second
        .list_threads(ListThreadsParams {
            page_size: 10,
            cursor: None,
            sort_key: ThreadSortKey::CreatedAt,
            sort_direction: SortDirection::Asc,
            allowed_sources: Vec::new(),
            model_providers: Some(Vec::new()),
            cwd_filters: None,
            archived: true,
            search_term: None,
            relation_filter: None,
            use_state_db_only: true,
        })
        .await?;
    assert_eq!(listed.items.len(), 1);
    let children = second
        .list_threads(ListThreadsParams {
            relation_filter: Some(ThreadRelationFilter::DirectChildrenOf(source.legacy_id)),
            ..ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Asc,
                allowed_sources: Vec::new(),
                model_providers: Some(Vec::new()),
                cwd_filters: None,
                archived: true,
                search_term: None,
                relation_filter: None,
                use_state_db_only: true,
            }
        })
        .await?;
    assert_eq!(
        serde_json::to_value(&children.items)?,
        serde_json::to_value(&listed.items)?
    );
    let searched = replica
        .search_threads(SearchThreadsParams {
            page_size: 10,
            cursor: None,
            sort_key: ThreadSortKey::CreatedAt,
            sort_direction: SortDirection::Asc,
            allowed_sources: Vec::new(),
            archived: true,
            search_term: "migration needle".to_string(),
        })
        .await?;
    assert_eq!(
        serde_json::to_value(&searched.items[0].thread)?,
        serde_json::to_value(&listed.items[0])?
    );
    let turn_params = ListTurnsParams {
        thread_id: source.thread_id,
        include_archived: true,
        cursor: None,
        page_size: 10,
        sort_direction: SortDirection::Asc,
        items_view: StoredTurnItemsView::Summary,
    };
    let turns = second.list_turns(turn_params.clone()).await?;
    assert_eq!(
        serde_json::to_value(&turns)?,
        serde_json::to_value(replica.list_turns(turn_params).await?)?
    );
    assert_eq!(turns.turns.len(), 1);
    let item_params = ListItemsParams {
        thread_id: source.thread_id,
        turn_id: None,
        include_archived: true,
        cursor: None,
        page_size: 10,
        sort_direction: SortDirection::Asc,
    };
    let items = replica.list_items(item_params.clone()).await?;
    assert_eq!(
        serde_json::to_value(&items)?,
        serde_json::to_value(second.list_items(item_params).await?)?
    );
    assert_eq!(items.items.len(), 1);
    let backfill = replica_pool.backfill_coordinator().state().await?;
    assert_eq!(backfill, second_pool.backfill_coordinator().state().await?);
    assert_eq!(
        backfill,
        BackfillState {
            status: BackfillStatus::Pending,
            last_watermark: None,
            last_success_at: None,
        }
    );
    let legacy = second
        .read_thread(ReadThreadParams {
            thread_id: source.legacy_id,
            include_archived: false,
            include_history: true,
        })
        .await?;
    assert_eq!(legacy.history_mode, ThreadHistoryMode::Legacy);
    assert_eq!(
        serde_json::to_value(legacy.history.ok_or("legacy history")?.items)?,
        serde_json::to_value(
            source
                .legacy_history
                .into_iter()
                .map(|line| line.item)
                .collect::<Vec<_>>()
        )?
    );
    let (pool, schema) = replica_pool.thread_store_connection();
    let source_ordinals: Vec<i64> = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT source_ordinal FROM \"{schema}\".thread_history WHERE thread_id = $1 ORDER BY ordinal"
    )))
    .bind(source.thread_id.to_string())
    .fetch_all(&pool)
    .await?;
    assert_eq!(source_ordinals, vec![0, 3, 10, 11, 12]);
    replica_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn runtime_state_thread_import_rolls_back_every_record_on_constraint_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let source = migration_source(LineageFixture::Orphan).await?;
    let source_before = (
        std::fs::read(&source.rollout_path)?,
        std::fs::read(source.config.home().join("config.toml"))?,
    );
    let fixture = PostgresThreadStoreFixture::new("runtime_migration_rollback")?;
    fixture.migrate().await?;
    let inventory =
        preflight_runtime_state_migration(source.config.clone(), fixture.config.clone()).await?;
    import_runtime_state_threads(
        &source.config,
        &fixture.config,
        &inventory,
        &codex_rollout::CanonicalRolloutHistoryReader,
    )
    .await
    .expect_err("orphan lineage must fail the import transaction");

    let replica_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let replica = PostgresThreadStore::new(&replica_pool);
    let thread_absent = matches!(
        replica
            .read_thread(ReadThreadParams {
                thread_id: source.thread_id,
                include_archived: true,
                include_history: true,
            })
            .await,
        Err(crate::ThreadStoreError::ThreadNotFound { .. })
    );
    let (pool, schema) = replica_pool.thread_store_connection();
    let migration_count: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM \"{schema}\".runtime_state_migration"
    )))
    .fetch_one(&pool)
    .await?;
    assert_eq!((thread_absent, migration_count), (true, 0));
    assert_eq!(
        (
            std::fs::read(&source.rollout_path)?,
            std::fs::read(source.config.home().join("config.toml"))?,
        ),
        source_before
    );
    replica_pool.close().await;
    fixture.cleanup().await?;
    Ok(())
}

pub(super) async fn migration_source(
    lineage: LineageFixture,
) -> Result<MigrationSource, Box<dyn std::error::Error>> {
    let source = tempfile::tempdir()?;
    let thread_id = ThreadId::from_string("019c84d0-3333-7777-8333-333333333333")?;
    let legacy_id = ThreadId::from_string("019c84d0-2222-7777-8222-222222222222")?;
    let rollout_path = source.path().join("sessions/2026/07/22/rollout.jsonl");
    let legacy_path = source.path().join("sessions/2026/07/21/legacy.jsonl");
    std::fs::create_dir_all(rollout_path.parent().ok_or("rollout parent")?)?;
    std::fs::create_dir_all(legacy_path.parent().ok_or("legacy rollout parent")?)?;
    let history = history(thread_id, source.path());
    std::fs::write(
        &rollout_path,
        history
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n")
            + "\n",
    )?;
    let legacy_history = legacy_history(legacy_id, source.path());
    std::fs::write(
        &legacy_path,
        serde_json::to_string(&legacy_history[0])? + "\n",
    )?;
    let runtime =
        codex_state::StateRuntime::init(source.path().to_path_buf(), "test-provider".to_string())
            .await?;
    open_thread_history_db(source.path()).await?.close().await;
    let created_at = Utc
        .with_ymd_and_hms(2026, 7, 22, 10, 0, 0)
        .single()
        .ok_or("time")?;
    let mut metadata = ThreadMetadataBuilder::new(
        thread_id,
        rollout_path.clone(),
        created_at,
        SessionSource::Cli,
    )
    .build("test-provider");
    metadata.history_mode = ThreadHistoryMode::Paginated;
    metadata.name = Some("Migrated thread".to_string());
    metadata.preview = Some("migration preview".to_string());
    metadata.tokens_used = 987;
    metadata.archived_at = Some(created_at + chrono::Duration::hours(1));
    metadata.git_sha = Some("abc987".to_string());
    metadata.git_branch = Some("migration".to_string());
    metadata.git_origin_url = Some("https://example.test/repo.git".to_string());
    runtime.upsert_thread(&metadata).await?;
    let mut legacy = ThreadMetadataBuilder::new(
        legacy_id,
        legacy_path,
        created_at - chrono::Duration::days(1),
        SessionSource::Cli,
    )
    .build("test-provider");
    legacy.title = "Legacy thread".to_string();
    runtime.upsert_thread(&legacy).await?;
    runtime
        .upsert_thread_spawn_edge(
            legacy_id,
            thread_id,
            DirectionalThreadSpawnEdgeStatus::Closed,
        )
        .await?;
    runtime
        .set_thread_memory_mode(thread_id, "disabled")
        .await?;
    if matches!(lineage, LineageFixture::Orphan) {
        runtime
            .upsert_thread_spawn_edge(
                thread_id,
                ThreadId::from_string("019c84d0-4444-7777-8444-444444444444")?,
                DirectionalThreadSpawnEdgeStatus::Open,
            )
            .await?;
    }
    runtime.close().await;
    std::fs::write(source.path().join("config.toml"), b"model = \"gpt-5\"\n")?;
    Ok(MigrationSource {
        config: SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.path())?),
        _temp: source,
        thread_id,
        history,
        legacy_id,
        legacy_history,
        rollout_path,
    })
}

fn legacy_history(thread_id: ThreadId, source: &std::path::Path) -> Vec<RolloutLine> {
    vec![RolloutLine {
        timestamp: "2026-07-21T10:00:00Z".to_string(),
        ordinal: None,
        item: RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                cwd: source.to_path_buf(),
                source: SessionSource::Cli,
                model_provider: Some("test-provider".to_string()),
                history_mode: ThreadHistoryMode::Legacy,
                ..SessionMeta::default()
            },
            git: None,
        }),
    }]
}

fn history(thread_id: ThreadId, source: &std::path::Path) -> Vec<RolloutLine> {
    let ancestor =
        ThreadId::from_string("019c84d0-2222-7777-8222-222222222222").expect("ancestor thread id");
    let mut meta = SessionMeta {
        session_id: thread_id.into(),
        id: thread_id,
        forked_from_id: Some(ancestor),
        parent_thread_id: Some(ancestor),
        timestamp: "2026-07-22T10:00:00.123Z".to_string(),
        cwd: source.to_path_buf(),
        originator: "migration-test".to_string(),
        cli_version: "0.0.0".to_string(),
        source: SessionSource::Cli,
        model_provider: Some("test-provider".to_string()),
        memory_mode: Some("disabled".to_string()),
        history_mode: ThreadHistoryMode::Paginated,
        history_base: Some(HistoryPosition {
            thread_id: ancestor,
            end_ordinal_exclusive: 4,
            end_byte_offset: 256,
        }),
        ..SessionMeta::default()
    };
    meta.dynamic_tools = Some(vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
        name: "lookup".to_string(),
        description: "Lookup migration state".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        defer_loading: true,
    })]);
    let completed_item = completed_item(
        thread_id,
        "turn-migration",
        TurnItem::UserMessage(UserMessageItem {
            id: "user-migration".to_string(),
            client_id: None,
            content: vec![UserInput::Text {
                text: "migration needle".to_string(),
                text_elements: Vec::new(),
            }],
        }),
    );
    vec![
        RolloutLine {
            timestamp: "2026-07-22T10:00:00.123Z".to_string(),
            ordinal: Some(0),
            item: RolloutItem::SessionMeta(SessionMetaLine { meta, git: None }),
        },
        RolloutLine {
            timestamp: "2026-07-22T10:01:00.456Z".to_string(),
            ordinal: Some(3),
            item: RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                num_turns: 1,
            })),
        },
        RolloutLine {
            timestamp: "2026-07-22T10:02:00.000Z".to_string(),
            ordinal: Some(10),
            item: completed_item,
        },
        RolloutLine {
            timestamp: "2026-07-22T10:03:00.000Z".to_string(),
            ordinal: Some(11),
            item: turn_started("turn-migration", 10),
        },
        RolloutLine {
            timestamp: "2026-07-22T10:04:00.000Z".to_string(),
            ordinal: Some(12),
            item: turn_complete("turn-migration", 10, 20, None),
        },
    ]
}
