use crate::PostgresThreadStore;
use crate::ReadThreadParams;
use crate::ThreadStore;
use crate::postgres_contract_tests::PostgresThreadStoreFixture;
use chrono::TimeZone;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadRolledBackEvent;
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

enum LineageFixture {
    Valid,
    Orphan,
}

struct MigrationSource {
    _temp: tempfile::TempDir,
    config: SqliteConfig,
    thread_id: ThreadId,
    history: Vec<RolloutLine>,
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
    let replica = PostgresThreadStore::new(&replica_pool);
    let imported = replica
        .read_thread(ReadThreadParams {
            thread_id: source.thread_id,
            include_archived: false,
            include_history: true,
        })
        .await?;
    assert_eq!(imported.name.as_deref(), Some("Migrated thread"));
    assert_eq!(imported.preview, "migration preview");
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
    replica_pool.close().await;
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

async fn migration_source(
    lineage: LineageFixture,
) -> Result<MigrationSource, Box<dyn std::error::Error>> {
    let source = tempfile::tempdir()?;
    let thread_id = ThreadId::from_string("019c84d0-3333-7777-8333-333333333333")?;
    let rollout_path = source.path().join("sessions/2026/07/22/rollout.jsonl");
    std::fs::create_dir_all(rollout_path.parent().ok_or("rollout parent")?)?;
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
    runtime.upsert_thread(&metadata).await?;
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
        rollout_path,
    })
}

fn history(thread_id: ThreadId, source: &std::path::Path) -> Vec<RolloutLine> {
    vec![
        RolloutLine {
            timestamp: "2026-07-22T10:00:00.123Z".to_string(),
            ordinal: Some(0),
            item: RolloutItem::SessionMeta(SessionMetaLine {
                meta: SessionMeta {
                    session_id: thread_id.into(),
                    id: thread_id,
                    timestamp: "2026-07-22T10:00:00.123Z".to_string(),
                    cwd: source.to_path_buf(),
                    originator: "migration-test".to_string(),
                    cli_version: "0.0.0".to_string(),
                    source: SessionSource::Cli,
                    model_provider: Some("test-provider".to_string()),
                    memory_mode: Some("disabled".to_string()),
                    history_mode: ThreadHistoryMode::Paginated,
                    ..SessionMeta::default()
                },
                git: None,
            }),
        },
        RolloutLine {
            timestamp: "2026-07-22T10:01:00.456Z".to_string(),
            ordinal: Some(1),
            item: RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                num_turns: 1,
            })),
        },
    ]
}
