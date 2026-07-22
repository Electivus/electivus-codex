use crate::PostgresThreadStore;
use crate::ThreadStore;
use crate::postgres_contract_tests::PostgresThreadStoreFixture;
use crate::postgres_contract_tests::create_thread_params;
use crate::runtime_state_migration_contract_tests::LineageFixture;
use crate::runtime_state_migration_contract_tests::MigrationSource;
use crate::runtime_state_migration_contract_tests::migration_source;
use codex_protocol::ThreadId;
use codex_state::PostgresRuntimeStatePool;
use codex_state::RuntimeStateMigrationInventory;
use codex_state::RuntimeStateMigrationProgress;
use codex_state::preflight_runtime_state_migration;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn runtime_state_thread_retry_rejects_changed_destination()
-> Result<(), Box<dyn std::error::Error>> {
    let source = migration_source(LineageFixture::Valid).await?;
    for (label, table, assignment) in [
        (
            "metadata",
            "threads",
            "projection = projection || '{\"name\":\"tampered\"}'::jsonb",
        ),
        (
            "history",
            "thread_history",
            "recorded_at = recorded_at + INTERVAL '1 second'",
        ),
        ("lineage", "thread_spawn_edges", "status = 'open'"),
        ("turn", "thread_turns", "duration_ms = duration_ms + 1"),
        ("item", "thread_items", "created_at_ms = created_at_ms + 1"),
        (
            "search",
            "thread_search_content",
            "content = content || ' tampered'",
        ),
    ] {
        let fixture =
            PostgresThreadStoreFixture::new(&format!("runtime_migration_tampering_{label}"))?;
        fixture.migrate().await?;
        let inventory =
            preflight_runtime_state_migration(source.config.clone(), fixture.config.clone())
                .await?;
        import_threads(&source, &fixture, &inventory).await?;
        let pool = sqlx::PgPool::connect(&fixture.database_url).await?;
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE \"{}\".{table} SET {assignment}",
            fixture.schema
        )))
        .execute(&pool)
        .await?;
        pool.close().await;
        let retry = import_threads(&source, &fixture, &inventory).await;
        fixture.cleanup().await?;
        retry.expect_err("retry must reject changed destination state");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn runtime_state_thread_import_rejects_changed_config_without_editing_it()
-> Result<(), Box<dyn std::error::Error>> {
    let source = migration_source(LineageFixture::Valid).await?;
    let fixture = PostgresThreadStoreFixture::new("runtime_migration_config")?;
    fixture.migrate().await?;
    let inventory =
        preflight_runtime_state_migration(source.config.clone(), fixture.config.clone()).await?;
    let config_path = source.config.home().join("config.toml");
    std::fs::write(&config_path, b"model = \"changed-after-preflight\"\n")?;
    let changed_config = std::fs::read(&config_path)?;
    let result = import_threads(&source, &fixture, &inventory).await;
    assert_eq!(std::fs::read(config_path)?, changed_config);
    fixture.cleanup().await?;
    result.expect_err("changed config must invalidate migration preflight");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn runtime_state_thread_import_rejects_other_sources_nonempty_and_ready_destinations()
-> Result<(), Box<dyn std::error::Error>> {
    let first_source = migration_source(LineageFixture::Valid).await?;
    let second_source = migration_source(LineageFixture::Valid).await?;
    let conflict = PostgresThreadStoreFixture::new("runtime_migration_source_conflict")?;
    conflict.migrate().await?;
    let first_inventory =
        preflight_runtime_state_migration(first_source.config.clone(), conflict.config.clone())
            .await?;
    let second_inventory =
        preflight_runtime_state_migration(second_source.config.clone(), conflict.config.clone())
            .await?;
    import_threads(&first_source, &conflict, &first_inventory).await?;
    import_threads(&second_source, &conflict, &second_inventory)
        .await
        .expect_err("another source must not reuse a migration namespace");
    conflict.cleanup().await?;

    let nonempty = PostgresThreadStoreFixture::new("runtime_migration_nonempty")?;
    nonempty.migrate().await?;
    let inventory =
        preflight_runtime_state_migration(first_source.config.clone(), nonempty.config.clone())
            .await?;
    let runtime_pool = PostgresRuntimeStatePool::connect(nonempty.config.clone()).await?;
    PostgresThreadStore::new(&runtime_pool)
        .create_thread(create_thread_params(ThreadId::from_string(
            "019c84d0-5555-7777-8555-555555555555",
        )?))
        .await?;
    import_threads(&first_source, &nonempty, &inventory)
        .await
        .expect_err("a write after preflight must make the destination ineligible");
    runtime_pool.close().await;
    nonempty.cleanup().await?;

    let ready = PostgresThreadStoreFixture::new("runtime_migration_ready")?;
    ready.migrate().await?;
    let inventory =
        preflight_runtime_state_migration(first_source.config.clone(), ready.config.clone())
            .await?;
    import_threads(&first_source, &ready, &inventory).await?;
    let pool = sqlx::PgPool::connect(&ready.database_url).await?;
    let schema = &ready.schema;
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE \"{schema}\".runtime_state_migration SET phase = 'ready', ready = TRUE"
    )))
    .execute(&pool)
    .await?;
    pool.close().await;
    import_threads(&first_source, &ready, &inventory)
        .await
        .expect_err("ready migrations must reject every retry");
    ready.cleanup().await?;
    Ok(())
}

async fn import_threads(
    source: &MigrationSource,
    fixture: &PostgresThreadStoreFixture,
    inventory: &RuntimeStateMigrationInventory,
) -> Result<RuntimeStateMigrationProgress, Box<dyn std::error::Error>> {
    let runtime_pool = PostgresRuntimeStatePool::connect(fixture.config.clone()).await?;
    let store = PostgresThreadStore::new(&runtime_pool);
    let result = codex_state::import_runtime_state_threads(
        &source.config,
        &fixture.config,
        inventory,
        &codex_rollout::CanonicalRolloutHistoryReader,
        &store,
    )
    .await;
    runtime_pool.close().await;
    Ok(result?)
}
