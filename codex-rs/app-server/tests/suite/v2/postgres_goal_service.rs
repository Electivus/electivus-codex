use anyhow::Result;
use codex_goal_extension::GoalObjectiveUpdate;
use codex_goal_extension::GoalPreviewUpdate;
use codex_goal_extension::GoalService;
use codex_goal_extension::GoalSetRequest;
use codex_goal_extension::GoalTokenBudgetUpdate;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_state::GoalAccountingMode;
use codex_state::GoalAccountingOutcome;
use codex_state::GoalAccountingRequest;
use codex_state::GoalAccountingTarget;
use codex_state::PostgresNamespaceAction;
use codex_state::PostgresNamespaceConfig;
use codex_state::PostgresPoolConfig;
use codex_state::PostgresRuntimeStatePool;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;
use uuid::Uuid;

const DATABASE_URL_ENV: &str = "CODEX_TEST_POSTGRES_URL";

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_goal_service_is_backend_neutral_across_replicas() -> Result<()> {
    let database_url = std::env::var(DATABASE_URL_ENV)?;
    let schema = format!("codex_app_server_goals_{}", Uuid::new_v4().simple());
    let config = PostgresNamespaceConfig::new(
        DATABASE_URL_ENV.to_string(),
        schema.clone(),
        PostgresPoolConfig::default(),
    )?;
    codex_state::manage_postgres_namespace(config.clone(), PostgresNamespaceAction::Migrate)
        .await?;
    mark_namespace_ready(&database_url, &schema).await?;
    let writer_pool = PostgresRuntimeStatePool::connect(config.clone()).await?;
    let reader_pool = PostgresRuntimeStatePool::connect(config).await?;
    let writer = writer_pool.goal_store();
    let reader = reader_pool.goal_store();
    let thread_id = ThreadId::new();
    seed_thread(&database_url, &schema, thread_id).await?;
    let service = GoalService::new();

    let created = service
        .set_thread_goal(
            &writer,
            GoalPreviewUpdate::Skip,
            GoalSetRequest {
                thread_id,
                objective: GoalObjectiveUpdate::Set("share goals through the app-server seam"),
                status: Some(ThreadGoalStatus::Active),
                token_budget: GoalTokenBudgetUpdate::Set(Some(100)),
            },
        )
        .await?;
    assert_eq!(
        service.get_thread_goal(&reader, thread_id).await?,
        Some(created.goal.clone())
    );

    let accounting = writer
        .account_thread_goal_usage(
            thread_id,
            GoalAccountingRequest {
                event_id: "app-server-cross-replica-accounting",
                time_delta_seconds: 2,
                token_delta: 40,
                mode: GoalAccountingMode::ActiveOnly,
                target: GoalAccountingTarget::CurrentGoal,
            },
        )
        .await?;
    assert!(matches!(accounting, GoalAccountingOutcome::Updated(_)));
    let writer_snapshot = service.get_thread_goal(&writer, thread_id).await?;
    assert_eq!(
        service.get_thread_goal(&reader, thread_id).await?,
        writer_snapshot
    );
    assert_eq!(
        writer_snapshot
            .as_ref()
            .map(|goal| (goal.tokens_used, goal.time_used_seconds)),
        Some((40, 2))
    );

    let updated = service
        .set_thread_goal(
            &reader,
            GoalPreviewUpdate::Skip,
            GoalSetRequest {
                thread_id,
                objective: GoalObjectiveUpdate::Set("update through another replica"),
                status: Some(ThreadGoalStatus::Paused),
                token_budget: GoalTokenBudgetUpdate::Set(Some(200)),
            },
        )
        .await?;
    assert_eq!(
        service.get_thread_goal(&writer, thread_id).await?,
        Some(updated.goal)
    );

    assert!(service.clear_thread_goal(&reader, thread_id).await?);
    assert_eq!(service.get_thread_goal(&writer, thread_id).await?, None);

    writer_pool.close().await;
    reader_pool.close().await;
    cleanup_schema(&database_url, &schema).await
}

async fn mark_namespace_ready(database_url: &str, schema: &str) -> Result<()> {
    let pool = sqlx::PgPool::connect(database_url).await?;
    let migration = format!("\"{schema}\".runtime_state_migration");
    let evidence = serde_json::json!({
        "sourceIdentity": "app-server-goal-contract",
        "sourceFingerprint": "app-server-goal-contract-fingerprint",
        "phase": "ready",
        "ready": true,
        "fencingToken": 4,
        "namespaceDigest": "app-server-goal-contract-digest",
        "globalReferentialIntegrityValidated": true,
        "canonicalThreadHistoryOrderingValidated": true,
    });
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {migration} (source_identity, source_fingerprint, phase, ready, \
         phase_evidence, fencing_token) VALUES ($1, $2, 'ready', TRUE, $3, 4)"
    )))
    .bind("app-server-goal-contract")
    .bind("app-server-goal-contract-fingerprint")
    .bind(evidence)
    .execute(&pool)
    .await?;
    pool.close().await;
    Ok(())
}

async fn seed_thread(database_url: &str, schema: &str, thread_id: ThreadId) -> Result<()> {
    let pool = sqlx::PgPool::connect(database_url).await?;
    let threads_table = format!("\"{schema}\".threads");
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {threads_table} (thread_id, projection, stream_version, fencing_token, \
         writer_id, writer_lease_expires_at, created_at, updated_at, recency_at) \
         VALUES ($1, '{{}}', 0, 1, 'goal-service-contract', CURRENT_TIMESTAMP, \
         CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
    )))
    .bind(thread_id.to_string())
    .execute(&pool)
    .await?;
    pool.close().await;
    Ok(())
}

async fn cleanup_schema(database_url: &str, schema: &str) -> Result<()> {
    let pool = sqlx::PgPool::connect(database_url).await?;
    sqlx::query(AssertSqlSafe(format!("DROP SCHEMA \"{schema}\" CASCADE")))
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}
