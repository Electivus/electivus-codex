#![allow(
    clippy::disallowed_methods,
    reason = "PostgreSQL tests connect only to PostgreSQL pools"
)]

use crate::ItemSortKey;
use crate::ListItemsParams;
use crate::ListThreadsParams;
use crate::ListTurnsParams;
use crate::LoadThreadHistoryParams;
use crate::LocalThreadStore;
use crate::LocalThreadStoreConfig;
use crate::PostgresThreadProjectionMaterializer;
use crate::PostgresThreadStore;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::SearchThreadsParams;
use crate::SortDirection;
use crate::StoredTurnItemsView;
use crate::ThreadPersistenceMetadata;
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
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use codex_state::BackfillClaimOutcome;
use codex_state::BackfillLeaseUpdate;
use codex_state::BackfillState;
use codex_state::DirectionalThreadSpawnEdgeStatus;
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
    pub(super) thread_id: ThreadId,
    history: Vec<RolloutLine>,
    legacy_id: ThreadId,
    legacy_history: Vec<RolloutLine>,
    rollout_only_id: ThreadId,
    rollout_only_history: Vec<RolloutLine>,
    pub(super) rollout_path: std::path::PathBuf,
    archived_at: chrono::DateTime<Utc>,
    public_view: ThreadPublicView,
    rollout_only_view: serde_json::Value,
    backfill: BackfillState,
    source_artifacts: Vec<(std::path::PathBuf, Vec<u8>, std::time::SystemTime)>,
}

#[derive(Debug, PartialEq)]
struct ThreadPublicView {
    active_threads: serde_json::Value,
    archived_threads: serde_json::Value,
    children: serde_json::Value,
    legacy_thread: serde_json::Value,
    current_thread: serde_json::Value,
    searched_threads: serde_json::Value,
    turns: serde_json::Value,
    items: serde_json::Value,
    legacy_history: serde_json::Value,
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_state_thread_import_is_visible_to_another_pool()
-> Result<(), Box<dyn std::error::Error>> {
    let source = migration_source(LineageFixture::Valid).await?;
    let fixture = PostgresThreadStoreFixture::new("runtime_migration")?;
    fixture.migrate().await?;
    let replica_pool = fixture.connect_pool().await?;
    let replica = PostgresThreadStore::new(replica_pool.clone(), fixture.schema.clone());
    let inventory =
        preflight_runtime_state_migration(source.config.clone(), fixture.config.clone()).await?;
    let projection_materializer = PostgresThreadProjectionMaterializer::new(&fixture.config);
    let progress = import_runtime_state_threads(
        &source.config,
        &fixture.config,
        &inventory,
        &codex_rollout::CanonicalRolloutHistoryReader,
        &projection_materializer,
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
            &projection_materializer,
        )
        .await?,
        progress
    );

    let second_pool = fixture.connect_pool().await?;
    let second = PostgresThreadStore::new(second_pool.clone(), fixture.schema.clone());
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
    assert_eq!(imported.archived_at, Some(source.archived_at));
    assert_eq!(imported.preview, "migration needle");
    assert_eq!(
        imported.repository_identity.as_deref(),
        Some("example.test/acme/repo")
    );
    assert_eq!(
        imported.token_usage,
        Some(TokenUsage {
            total_tokens: 987,
            ..Default::default()
        })
    );
    let imported_history = imported.history.as_ref().ok_or("history")?;
    assert_eq!(
        serde_json::to_value(&imported_history.items)?,
        serde_json::to_value(
            source
                .history
                .iter()
                .map(|line| line.item.clone())
                .collect::<Vec<_>>()
        )?
    );
    let session_meta = imported_history
        .items
        .iter()
        .find_map(|item| match item {
            RolloutItem::SessionMeta(meta) => Some(&meta.meta),
            _ => None,
        })
        .ok_or("session meta")?;
    assert_eq!(
        (
            session_meta.forked_from_id,
            session_meta.parent_thread_id,
            session_meta.memory_mode.as_deref(),
            session_meta.history_base,
            session_meta.dynamic_tools.as_ref().map(Vec::len),
        ),
        (
            Some(source.legacy_id),
            None,
            Some("disabled"),
            None,
            Some(1),
        )
    );
    assert!(imported_history.items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(event)) if event.num_turns == 1
        )
    }));
    let listed = second
        .list_threads(ListThreadsParams {
            page_size: 10,
            cursor: None,
            sort_key: ThreadSortKey::CreatedAt,
            sort_direction: SortDirection::Asc,
            allowed_sources: Vec::new(),
            model_providers: Some(Vec::new()),
            location_filter: crate::ThreadLocationFilter::Unrestricted,
            section: None,
            project_id: None,
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
                location_filter: crate::ThreadLocationFilter::Unrestricted,
                section: None,
                project_id: None,
                archived: true,
                search_term: None,
                relation_filter: None,
                use_state_db_only: true,
            }
        })
        .await?;
    let mut expected_children = listed.items.clone();
    for child in &mut expected_children {
        child.parent_thread_id = Some(source.legacy_id);
    }
    assert_eq!(
        serde_json::to_value(&children.items)?,
        serde_json::to_value(expected_children)?
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
        sort_key: ItemSortKey::CreatedAtOrdinal,
        after_updated_at_ordinal: None,
    };
    let items = replica.list_items(item_params.clone()).await?;
    assert_eq!(
        serde_json::to_value(&items)?,
        serde_json::to_value(second.list_items(item_params).await?)?
    );
    assert_eq!(items.items.len(), 1);
    let backfill_query = format!(
        "SELECT status, last_watermark, last_success_at FROM \"{}\".backfill_state WHERE id = 1",
        fixture.schema
    );
    let backfill: (String, Option<String>, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as(AssertSqlSafe(backfill_query.clone()))
            .fetch_one(&replica_pool)
            .await?;
    let backfill_from_second: (String, Option<String>, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as(AssertSqlSafe(backfill_query))
            .fetch_one(&second_pool)
            .await?;
    let expected_backfill = (
        source.backfill.status.as_str().to_string(),
        source.backfill.last_watermark.clone(),
        source.backfill.last_success_at,
    );
    assert_eq!(
        (backfill, backfill_from_second),
        (expected_backfill.clone(), expected_backfill)
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
                .iter()
                .map(|line| line.item.clone())
                .collect::<Vec<_>>()
        )?
    );
    let pool = &replica_pool;
    let schema = &fixture.schema;
    let source_ordinals: Vec<i64> = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT source_ordinal FROM \"{schema}\".thread_history WHERE thread_id = $1 ORDER BY ordinal"
    )))
    .bind(source.thread_id.to_string())
    .fetch_all(pool)
    .await?;
    assert_eq!(source_ordinals, vec![0, 1, 2, 3, 4, 5]);
    let pollution_overrides: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM \"{schema}\".memory_thread_mode_overrides"
    )))
    .fetch_one(pool)
    .await?;
    assert_eq!(pollution_overrides, 2);
    assert_eq!(
        thread_public_view(&replica, source.legacy_id, source.thread_id).await?,
        source.public_view
    );
    let rollout_only = second
        .read_thread(ReadThreadParams {
            thread_id: source.rollout_only_id,
            include_archived: true,
            include_history: true,
        })
        .await?;
    assert_eq!(rollout_only.name.as_deref(), Some("Index-only thread name"));
    assert_eq!(normalized_value(&rollout_only)?, source.rollout_only_view);
    assert_eq!(
        serde_json::to_value(rollout_only.history.ok_or("rollout-only history")?.items)?,
        serde_json::to_value(
            source
                .rollout_only_history
                .iter()
                .map(|line| line.item.clone())
                .collect::<Vec<_>>()
        )?
    );
    assert_eq!(source_artifacts(&source)?, source.source_artifacts);
    assert_eq!(
        import_runtime_state_threads(
            &source.config,
            &fixture.config,
            &inventory,
            &codex_rollout::CanonicalRolloutHistoryReader,
            &projection_materializer,
        )
        .await?,
        progress
    );
    replica_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_state_thread_import_rolls_back_every_record_on_constraint_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let source = migration_source(LineageFixture::Orphan).await?;
    let source_before = (
        std::fs::read(&source.rollout_path)?,
        std::fs::read(source.config.home().join("config.toml"))?,
    );
    let fixture = PostgresThreadStoreFixture::new("runtime_migration_rollback")?;
    fixture.migrate().await?;
    let replica_pool = fixture.connect_pool().await?;
    let replica = PostgresThreadStore::new(replica_pool.clone(), fixture.schema.clone());
    let inventory =
        preflight_runtime_state_migration(source.config.clone(), fixture.config.clone()).await?;
    let projection_materializer = PostgresThreadProjectionMaterializer::new(&fixture.config);
    import_runtime_state_threads(
        &source.config,
        &fixture.config,
        &inventory,
        &codex_rollout::CanonicalRolloutHistoryReader,
        &projection_materializer,
    )
    .await
    .expect_err("orphan lineage must fail the import transaction");

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
    let pool = &replica_pool;
    let schema = &fixture.schema;
    let migration_count: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM \"{schema}\".runtime_state_migration"
    )))
    .fetch_one(pool)
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

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_state_migration_reports_ready_only_after_every_phase()
-> Result<(), Box<dyn std::error::Error>> {
    let source = migration_source(LineageFixture::Valid).await?;
    let source_before = source_artifacts(&source)?;
    let fixture = PostgresThreadStoreFixture::new("runtime_migration_ready_report")?;
    fixture.migrate().await?;

    let report = run_migration(&source, &fixture).await?;

    assert_eq!(
        (report.fencing_token(), report.destination_schema()),
        (4, fixture.schema.as_str())
    );
    assert_eq!(report.evidence()["threads"], serde_json::Value::from(3));
    assert_eq!(
        report.evidence()["memoryArtifacts"],
        serde_json::Value::from(1)
    );
    assert_eq!(
        [
            report.evidence()["threadsContentHash"]
                .as_str()
                .unwrap_or_default()
                .len(),
            report.evidence()["historyContentHash"]
                .as_str()
                .unwrap_or_default()
                .len(),
            report.evidence()["threadCoordinationContentHash"]
                .as_str()
                .unwrap_or_default()
                .len(),
        ],
        [64, 64, 64]
    );
    assert_eq!(source_artifacts(&source)?, source_before);
    fixture.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_state_migration_rebuilds_projections_without_thread_history_database()
-> Result<(), Box<dyn std::error::Error>> {
    let source = migration_source(LineageFixture::Valid).await?;
    std::fs::remove_file(source.config.thread_history_db_path())?;
    let source_before = source_artifacts(&source)?;
    let fixture = PostgresThreadStoreFixture::new("runtime_migration_without_history_db")?;
    fixture.migrate().await?;

    let report = run_migration(&source, &fixture).await?;

    assert_eq!(report.fencing_token(), 4);
    let pool = fixture.connect_pool().await?;
    let store = PostgresThreadStore::new(pool.clone(), fixture.schema.clone());
    assert_eq!(
        thread_public_view(&store, source.legacy_id, source.thread_id).await?,
        source.public_view
    );
    assert_eq!(source_artifacts(&source)?, source_before);
    pool.close().await;
    fixture.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_state_migration_resumes_an_interrupted_phase_chain()
-> Result<(), Box<dyn std::error::Error>> {
    let source = migration_source(LineageFixture::Valid).await?;
    let source_before = source_artifacts(&source)?;
    let fixture = PostgresThreadStoreFixture::new("runtime_migration_resume")?;
    fixture.migrate().await?;
    let inventory =
        preflight_runtime_state_migration(source.config.clone(), fixture.config.clone()).await?;
    let progress = import_threads(&source, &fixture, &inventory).await?;
    assert_eq!(
        progress.phase(),
        RuntimeStateMigrationPhase::ThreadsImported
    );

    let report = run_migration(&source, &fixture).await?;

    assert_eq!(report.fencing_token(), 4);
    assert_eq!(source_artifacts(&source)?, source_before);
    fixture.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_state_migration_validation_failure_stays_not_ready()
-> Result<(), Box<dyn std::error::Error>> {
    let source = migration_source(LineageFixture::Valid).await?;
    let source_before = source_artifacts(&source)?;
    let fixture = PostgresThreadStoreFixture::new("runtime_migration_final_validation")?;
    fixture.migrate().await?;
    let inventory =
        preflight_runtime_state_migration(source.config.clone(), fixture.config.clone()).await?;
    import_threads(&source, &fixture, &inventory).await?;
    codex_state::import_runtime_state_operational(&source.config, &fixture.config, &inventory)
        .await?;
    codex_state::import_runtime_state_memory(&source.config, &fixture.config, &inventory).await?;
    let pool = sqlx::PgPool::connect(&fixture.database_url).await?;
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE \"{}\".thread_history SET recorded_at = recorded_at + INTERVAL '1 second'",
        fixture.schema
    )))
    .execute(&pool)
    .await?;

    run_migration(&source, &fixture)
        .await
        .expect_err("changed content must prevent final readiness");

    let state: (String, bool, i64) = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT phase, ready, fencing_token FROM \"{}\".runtime_state_migration",
        fixture.schema
    )))
    .fetch_one(&pool)
    .await?;
    assert_eq!(state, ("memory_imported".to_string(), false, 3));
    assert_eq!(source_artifacts(&source)?, source_before);
    pool.close().await;
    fixture.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_state_migration_rejects_a_nonempty_destination()
-> Result<(), Box<dyn std::error::Error>> {
    let source = migration_source(LineageFixture::Valid).await?;
    let source_before = source_artifacts(&source)?;
    let fixture = PostgresThreadStoreFixture::new("runtime_migration_nonempty_command")?;
    fixture.migrate().await?;
    let runtime_pool = fixture.connect_pool().await?;
    PostgresThreadStore::new(runtime_pool.clone(), fixture.schema.clone())
        .create_thread(crate::postgres_contract_tests::create_thread_params(
            ThreadId::from_string("019c84d0-6666-7777-8666-666666666666")?,
        ))
        .await?;

    run_migration(&source, &fixture)
        .await
        .expect_err("a non-empty destination must be rejected");

    let pool = &runtime_pool;
    let schema = &fixture.schema;
    let ready_count: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM \"{schema}\".runtime_state_migration WHERE ready"
    )))
    .fetch_one(pool)
    .await?;
    assert_eq!(ready_count, 0);
    assert_eq!(source_artifacts(&source)?, source_before);
    runtime_pool.close().await;
    fixture.cleanup().await?;
    Ok(())
}

async fn import_threads(
    source: &MigrationSource,
    fixture: &PostgresThreadStoreFixture,
    inventory: &codex_state::RuntimeStateMigrationInventory,
) -> Result<codex_state::RuntimeStateMigrationProgress, Box<dyn std::error::Error>> {
    let projection_materializer = PostgresThreadProjectionMaterializer::new(&fixture.config);
    Ok(codex_state::import_runtime_state_threads(
        &source.config,
        &fixture.config,
        inventory,
        &codex_rollout::CanonicalRolloutHistoryReader,
        &projection_materializer,
    )
    .await?)
}

async fn run_migration(
    source: &MigrationSource,
    fixture: &PostgresThreadStoreFixture,
) -> Result<codex_state::RuntimeStateMigrationReport, Box<dyn std::error::Error>> {
    let projection_materializer = PostgresThreadProjectionMaterializer::new(&fixture.config);
    Ok(codex_state::migrate_runtime_state(
        source.config.clone(),
        fixture.config.clone(),
        &codex_rollout::CanonicalRolloutHistoryReader,
        &projection_materializer,
    )
    .await?)
}

// The destination is `String` before the catch-up and `SanitizedGitUrl` after it.
#[allow(clippy::useless_conversion)]
pub(super) async fn migration_source(
    lineage: LineageFixture,
) -> Result<MigrationSource, Box<dyn std::error::Error>> {
    let source = tempfile::tempdir()?;
    let thread_id = ThreadId::from_string("019c84d0-3333-7777-8333-333333333333")?;
    let legacy_id = ThreadId::from_string("019c84d0-2222-7777-8222-222222222222")?;
    let rollout_only_id = ThreadId::from_string("019c84d0-1111-7777-8111-111111111111")?;
    let rollout_path = source.path().join(format!(
        "archived_sessions/rollout-2026-07-22T10-00-00-{thread_id}.jsonl"
    ));
    let legacy_path = source.path().join("sessions/2026/07/21/legacy.jsonl");
    let rollout_only_path = source.path().join(format!(
        "archived_sessions/rollout-2026-07-21T10-00-00-{rollout_only_id}.jsonl.zst"
    ));
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
    let mut rollout_only_history = self::legacy_history(rollout_only_id, source.path());
    if let RolloutItem::SessionMeta(meta) = &mut rollout_only_history[0].item {
        meta.meta.memory_mode = Some("polluted".to_string());
    }
    let rollout_only_json = rollout_only_history
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n")
        + "\n";
    std::fs::write(
        &rollout_only_path,
        zstd::stream::encode_all(rollout_only_json.as_bytes(), /*level*/ 0)?,
    )?;
    codex_rollout::append_thread_name(source.path(), rollout_only_id, "Index-only thread name")
        .await?;
    let runtime = codex_state::StateRuntime::init_sqlite(
        source.path().to_path_buf(),
        "test-provider".to_string(),
    )
    .await?;
    let config = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.path())?);
    open_thread_history_db(&config).await?.close().await;
    let created_at = Utc
        .with_ymd_and_hms(2026, 7, 22, 10, 0, 0)
        .single()
        .ok_or("time")?
        + chrono::Duration::milliseconds(123);
    let mut metadata = ThreadMetadataBuilder::new(
        thread_id,
        rollout_path.clone(),
        created_at,
        SessionSource::Cli,
    )
    .build("test-provider");
    metadata.history_mode = ThreadHistoryMode::Paginated;
    metadata.name = Some("Migrated thread".to_string());
    metadata.preview = Some("migration needle".to_string());
    metadata.first_user_message = metadata.preview.clone();
    metadata.tokens_used = 987;
    let archived_at = Utc
        .with_ymd_and_hms(2026, 7, 22, 8, 0, 0)
        .single()
        .ok_or("archive time")?;
    metadata.archived_at = Some(archived_at);
    metadata.cwd = source.path().to_path_buf();
    metadata.cli_version = "0.0.0".to_string();
    metadata.git_sha = Some("abc987".to_string());
    metadata.git_branch = Some("migration".to_string());
    metadata.git_origin_url = Some(
        "https://example.test/acme/repo.git"
            .to_string()
            .try_into()
            .expect("valid git remote URL"),
    );
    runtime.upsert_thread(&metadata).await?;
    let mut legacy = ThreadMetadataBuilder::new(
        legacy_id,
        legacy_path,
        created_at - chrono::Duration::days(1),
        SessionSource::Cli,
    )
    .build("test-provider");
    legacy.title = "Legacy thread".to_string();
    legacy.first_user_message = Some("legacy preview".to_string());
    legacy.preview = legacy.first_user_message.clone();
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
    let local = LocalThreadStore::new(
        LocalThreadStoreConfig {
            codex_home: source.path().to_path_buf(),
            sqlite: config.clone(),
            default_model_provider_id: "test-provider".to_string(),
        },
        Some(runtime.clone()),
    );
    local
        .resume_thread(ResumeThreadParams {
            thread_id,
            rollout_path: Some(rollout_path.clone()),
            history: None,
            include_archived: true,
            metadata: ThreadPersistenceMetadata {
                cwd: Some(source.path().to_path_buf()),
                model_provider: "test-provider".to_string(),
                memory_mode: ThreadMemoryMode::Disabled,
            },
        })
        .await?;
    local.shutdown_thread(thread_id).await?;
    let coordinator = runtime.backfill_coordinator();
    let lease = match coordinator
        .try_claim("migration-source", std::time::Duration::from_secs(3600))
        .await?
    {
        BackfillClaimOutcome::Claimed { lease, .. } => lease,
        outcome => return Err(format!("unexpected backfill claim: {outcome:?}").into()),
    };
    assert_eq!(
        coordinator
            .checkpoint(
                &lease,
                "archived_sessions/rollout.jsonl",
                std::time::Duration::from_secs(3600),
            )
            .await?,
        BackfillLeaseUpdate::Applied
    );
    let public_view = thread_public_view(&local, legacy_id, thread_id).await?;
    runtime
        .set_thread_memory_mode(thread_id, "polluted")
        .await?;
    runtime.delete_thread(rollout_only_id).await?;
    let backfill = runtime.backfill_coordinator().state().await?;
    drop(local);
    runtime.close().await;
    let rollout_local = LocalThreadStore::new(
        LocalThreadStoreConfig {
            codex_home: source.path().to_path_buf(),
            sqlite: config.clone(),
            default_model_provider_id: "test-provider".to_string(),
        },
        /*state_db*/ None,
    );
    let rollout_only_view = normalized_value(
        rollout_local
            .read_thread(ReadThreadParams {
                thread_id: rollout_only_id,
                include_archived: true,
                include_history: true,
            })
            .await?,
    )?;
    open_thread_history_db(&config).await?.close().await;
    std::fs::write(source.path().join("config.toml"), b"model = \"gpt-5\"\n")?;
    std::fs::create_dir_all(source.path().join("memories"))?;
    std::fs::write(
        source.path().join("memories/MEMORY.md"),
        b"# Preserved migration memory\n",
    )?;
    let mut migration_source = MigrationSource {
        config,
        _temp: source,
        thread_id,
        history,
        legacy_id,
        legacy_history,
        rollout_only_id,
        rollout_only_history,
        rollout_path,
        archived_at,
        public_view,
        rollout_only_view,
        backfill,
        source_artifacts: Vec::new(),
    };
    migration_source.source_artifacts = source_artifacts(&migration_source)?;
    Ok(migration_source)
}

fn source_artifacts(
    source: &MigrationSource,
) -> std::io::Result<Vec<(std::path::PathBuf, Vec<u8>, std::time::SystemTime)>> {
    let mut directories = vec![source.config.home().to_path_buf()];
    let mut artifacts = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                directories.push(entry.path());
            } else if metadata.is_file() {
                artifacts.push((
                    entry
                        .path()
                        .strip_prefix(source.config.home())
                        .unwrap()
                        .to_path_buf(),
                    std::fs::read(entry.path())?,
                    metadata.modified()?,
                ));
            }
        }
    }
    artifacts.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(artifacts)
}

async fn thread_public_view(
    store: &impl ThreadStore,
    legacy_id: ThreadId,
    current_id: ThreadId,
) -> Result<ThreadPublicView, Box<dyn std::error::Error>> {
    let active_threads = store.list_threads(list_params(/*archived*/ false)).await?;
    let archived_threads = store.list_threads(list_params(/*archived*/ true)).await?;
    let children = store
        .list_threads(ListThreadsParams {
            relation_filter: Some(ThreadRelationFilter::DirectChildrenOf(legacy_id)),
            ..list_params(/*archived*/ true)
        })
        .await?;
    let legacy_thread = store
        .read_thread(ReadThreadParams {
            thread_id: legacy_id,
            include_archived: false,
            include_history: true,
        })
        .await?;
    let current_thread = store
        .read_thread(ReadThreadParams {
            thread_id: current_id,
            include_archived: true,
            include_history: false,
        })
        .await?;
    let searched_threads = store
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
    let turns = store
        .list_turns(ListTurnsParams {
            thread_id: current_id,
            include_archived: true,
            cursor: None,
            page_size: 10,
            sort_direction: SortDirection::Asc,
            items_view: StoredTurnItemsView::Summary,
        })
        .await?;
    let items = store
        .list_items(ListItemsParams {
            thread_id: current_id,
            turn_id: None,
            include_archived: true,
            cursor: None,
            page_size: 10,
            sort_direction: SortDirection::Asc,
            sort_key: ItemSortKey::CreatedAtOrdinal,
            after_updated_at_ordinal: None,
        })
        .await?;
    let legacy_history = store
        .load_history(LoadThreadHistoryParams {
            thread_id: legacy_id,
            include_archived: false,
        })
        .await?;
    Ok(ThreadPublicView {
        active_threads: normalized_value(active_threads)?,
        archived_threads: normalized_value(archived_threads)?,
        children: normalized_value(children)?,
        legacy_thread: normalized_value(legacy_thread)?,
        current_thread: normalized_value(current_thread)?,
        searched_threads: normalized_value(searched_threads)?,
        turns: normalized_value(turns)?,
        items: normalized_value(items)?,
        legacy_history: normalized_value(legacy_history)?,
    })
}

fn list_params(archived: bool) -> ListThreadsParams {
    ListThreadsParams {
        page_size: 10,
        cursor: None,
        sort_key: ThreadSortKey::CreatedAt,
        sort_direction: SortDirection::Asc,
        allowed_sources: Vec::new(),
        model_providers: Some(Vec::new()),
        location_filter: crate::ThreadLocationFilter::Unrestricted,
        section: None,
        project_id: None,
        archived,
        search_term: None,
        relation_filter: None,
        use_state_db_only: true,
    }
}

fn normalized_value(value: impl serde::Serialize) -> Result<serde_json::Value, serde_json::Error> {
    let mut value = serde_json::to_value(value)?;
    normalize_backend_paths(&mut value);
    Ok(value)
}

fn normalize_backend_paths(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                normalize_backend_paths(value);
            }
        }
        serde_json::Value::Object(fields) => {
            if let Some(rollout_path) = fields.get_mut("rollout_path") {
                *rollout_path = serde_json::Value::Null;
            }
            if let Some(serde_json::Value::Array(bytes)) = fields.get_mut("item_json") {
                let bytes = bytes
                    .iter()
                    .filter_map(serde_json::Value::as_u64)
                    .filter_map(|byte| u8::try_from(byte).ok())
                    .collect::<Vec<_>>();
                if let Ok(item) = serde_json::from_slice(bytes.as_slice()) {
                    fields.insert("item_json".to_string(), item);
                }
            }
            for value in fields.values_mut() {
                normalize_backend_paths(value);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

fn legacy_history(thread_id: ThreadId, source: &std::path::Path) -> Vec<RolloutLine> {
    vec![RolloutLine {
        timestamp: "2026-07-21T10:00:00Z".to_string(),
        ordinal: None,
        item: RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                timestamp: "2026-07-21T10:00:00Z".to_string(),
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
        parent_thread_id: None,
        timestamp: "2026-07-22T10:00:00.123Z".to_string(),
        cwd: source.to_path_buf(),
        originator: "migration-test".to_string(),
        cli_version: "0.0.0".to_string(),
        source: SessionSource::Cli,
        model_provider: Some("test-provider".to_string()),
        memory_mode: Some("disabled".to_string()),
        history_mode: ThreadHistoryMode::Paginated,
        history_base: None,
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
            ordinal: Some(1),
            item: RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                num_turns: 1,
            })),
        },
        RolloutLine {
            timestamp: "2026-07-22T10:02:00.000Z".to_string(),
            ordinal: Some(2),
            item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: "migration needle".to_string(),
                ..Default::default()
            })),
        },
        RolloutLine {
            timestamp: "2026-07-22T10:03:00.000Z".to_string(),
            ordinal: Some(3),
            item: turn_started("turn-migration", /*started_at*/ 10),
        },
        RolloutLine {
            timestamp: "2026-07-22T10:04:00.000Z".to_string(),
            ordinal: Some(4),
            item: completed_item,
        },
        RolloutLine {
            timestamp: "2026-07-22T10:05:00.000Z".to_string(),
            ordinal: Some(5),
            item: turn_complete(
                "turn-migration",
                /*started_at*/ 10,
                /*completed_at*/ 20,
                /*error*/ None,
            ),
        },
    ]
}
