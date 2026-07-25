use crate::PostgresThreadProjectionMaterializer;
use crate::PostgresThreadStore;
use crate::ThreadStore;
use crate::postgres_contract_tests::PostgresThreadStoreFixture;
use crate::postgres_contract_tests::create_thread_params;
use crate::runtime_state_migration_contract_tests::LineageFixture;
use crate::runtime_state_migration_contract_tests::MigrationSource;
use crate::runtime_state_migration_contract_tests::migration_source;
use codex_protocol::ThreadId;
use codex_state::RuntimeStateMigrationInventory;
use codex_state::RuntimeStateMigrationProgress;
use codex_state::preflight_runtime_state_migration;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_state_thread_import_rejects_discarded_canonical_records()
-> Result<(), Box<dyn std::error::Error>> {
    let source = migration_source(LineageFixture::Valid).await?;
    let mut rollout = std::fs::read_to_string(&source.rollout_path)?;
    rollout.push_str(
        "{\"timestamp\":\"2026-07-22T10:06:00Z\",\"type\":\"future_item\",\"payload\":{}}\n",
    );
    std::fs::write(&source.rollout_path, &rollout)?;
    let fixture = PostgresThreadStoreFixture::new("runtime_migration_rejected_record")?;
    fixture.migrate().await?;
    let inventory =
        preflight_runtime_state_migration(source.config.clone(), fixture.config.clone()).await?;

    import_threads(&source, &fixture, &inventory)
        .await
        .expect_err("discarding a canonical rollout record must block migration");
    assert_eq!(std::fs::read_to_string(&source.rollout_path)?, rollout);
    fixture.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_state_thread_retry_rejects_changed_destination()
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
        (
            "migration_phase",
            "runtime_state_migration",
            "phase = 'operational_imported'",
        ),
        (
            "migration_fence",
            "runtime_state_migration",
            "fencing_token = fencing_token + 1",
        ),
        (
            "migration_evidence_extra",
            "runtime_state_migration",
            "phase_evidence = phase_evidence || '{\"unexpected\":true}'::jsonb",
        ),
    ] {
        let fixture = PostgresThreadStoreFixture::new(&format!("mig_tamper_{label}"))?;
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
async fn postgres_contract_runtime_state_thread_import_diagnoses_stale_local_projections()
-> Result<(), Box<dyn std::error::Error>> {
    let lagging_source = migration_source(LineageFixture::Valid).await?;
    assert_stale_projection(
        &lagging_source,
        "runtime_migration_lagging_projection",
        "UPDATE thread_history_projection_state \
         SET next_rollout_ordinal = next_rollout_ordinal - 1 WHERE thread_id = ?",
        "projection cursor",
    )
    .await?;

    let rollback_source = migration_source(LineageFixture::Valid).await?;
    assert_stale_projection(
        &rollback_source,
        "runtime_migration_rollback_projection",
        "INSERT INTO thread_turns \
         (thread_id, turn_id, rollout_ordinal, status) VALUES (?, 'rolled-back-turn', 0, 'completed')",
        "differ from eager Canonical Thread History projection",
    )
    .await?;
    Ok(())
}

async fn assert_stale_projection(
    source: &MigrationSource,
    destination: &str,
    mutation: &'static str,
    expected_error: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let history_pool = codex_state::open_thread_history_db(&source.config).await?;
    sqlx::query(mutation)
        .bind(source.thread_id.to_string())
        .execute(&history_pool)
        .await?;
    history_pool.close().await;
    let fixture = PostgresThreadStoreFixture::new(destination)?;
    fixture.migrate().await?;
    let inventory =
        preflight_runtime_state_migration(source.config.clone(), fixture.config.clone()).await?;
    let error = import_threads(source, &fixture, &inventory)
        .await
        .expect_err("a stale local projection must be rejected");
    assert!(format!("{error:#}").contains(expected_error), "{error:#}");
    fixture.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_state_thread_import_rejects_changed_config_without_editing_it()
-> Result<(), Box<dyn std::error::Error>> {
    let source = migration_source(LineageFixture::Valid).await?;
    let fixture = PostgresThreadStoreFixture::new("runtime_migration_config")?;
    fixture.migrate().await?;
    let inventory =
        preflight_runtime_state_migration(source.config.clone(), fixture.config.clone()).await?;
    let config_path = source.config.home().join("config.toml");
    let original_config = std::fs::read(&config_path)?;
    std::fs::write(&config_path, b"model = \"changed-after-preflight\"\n")?;
    let changed_config = std::fs::read(&config_path)?;
    import_threads(&source, &fixture, &inventory)
        .await
        .expect_err("changed config must invalidate migration preflight");
    assert_eq!(std::fs::read(config_path)?, changed_config);
    std::fs::write(source.config.home().join("config.toml"), original_config)?;

    let session_index_path = source.config.home().join("session_index.jsonl");
    std::fs::write(&session_index_path, b"changed-after-preflight\n")?;
    let changed_session_index = std::fs::read(&session_index_path)?;
    import_threads(&source, &fixture, &inventory)
        .await
        .expect_err("changed session index must invalidate migration preflight");
    assert_eq!(std::fs::read(session_index_path)?, changed_session_index);
    fixture.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_state_thread_import_rejects_other_sources_nonempty_and_ready_destinations()
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
    let runtime_pool = nonempty.connect_pool().await?;
    PostgresThreadStore::new(runtime_pool.clone(), nonempty.schema.clone())
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
