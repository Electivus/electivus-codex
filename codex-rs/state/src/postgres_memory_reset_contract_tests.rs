use super::PostgresNamespaceAction;
use super::qualified_table;
use super::test_support::PostgresContractFixture;
use super::test_support::test_database_url;
use crate::MemoryArtifact;
use crate::MemoryArtifactSet;
use crate::MemoryWorkspaceMaterialization;
use crate::Phase2JobClaimOutcome;
use crate::Stage1JobClaimOutcome;
use crate::runtime::memory_store_phase2_success_contract_tests::phase2_success_thread_ids;
use crate::runtime::memory_store_phase2_success_contract_tests::seed_postgres_phase2_success_threads;
use crate::runtime::memory_store_reset_contract_tests::MemoryResetSnapshot;
use crate::runtime::memory_store_reset_contract_tests::run_memory_reset_contract;
use anyhow::Result;
use pretty_assertions::assert_eq;
use serde_json::json;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use sqlx::Row;

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_cross_pool_memory_reset_satisfies_shared_contract() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "memory_reset")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let setup_pool = fixture.connect_pool().await?;
    let writer = crate::MemoryStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    let resetter = crate::MemoryStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    let thread_ids = phase2_success_thread_ids()?;
    seed_postgres_phase2_success_threads(&setup_pool, fixture.schema(), thread_ids).await?;
    let threads_table = qualified_table(fixture.schema(), "threads");
    let history_table = qualified_table(fixture.schema(), "thread_history");
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {threads_table} SET stream_version = 2 WHERE thread_id = $1"
    )))
    .bind(thread_ids[1].to_string())
    .execute(&setup_pool)
    .await?;
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {history_table} (thread_id, ordinal, item) VALUES ($1, 1, $2)"
    )))
    .bind(thread_ids[1].to_string())
    .bind(json!({
        "type": "session_meta",
        "payload": { "id": thread_ids[1], "memory_mode": "disabled" },
    }))
    .execute(&setup_pool)
    .await?;

    let age_pool = setup_pool.clone();
    let age_jobs_table = qualified_table(fixture.schema(), "memory_jobs");
    let snapshot_pool = setup_pool.clone();
    let snapshot_schema = fixture.schema().to_string();
    run_memory_reset_contract(
        &writer,
        &resetter,
        thread_ids[0],
        move || async move {
            sqlx::query(AssertSqlSafe(format!(
                "UPDATE {age_jobs_table} SET finished_at = 0 \
                 WHERE kind = 'memory_consolidate_global'"
            )))
            .execute(&age_pool)
            .await?;
            Ok(())
        },
        move || {
            let pool = snapshot_pool.clone();
            let schema = snapshot_schema.clone();
            async move { postgres_snapshot(&pool, &schema).await }
        },
        MemoryResetSnapshot {
            memory_jobs: 2,
            outputs: 1,
            used_outputs: 1,
            selected_outputs: 1,
            generations: 1,
            artifacts: 1,
            active_generations: 1,
            threads: 2,
            history_items: 3,
            disabled_modes: 1,
            pollution_overrides: 1,
        },
        MemoryWorkspaceMaterialization::Clear,
    )
    .await?;

    let source_updated_at = 1_700_000_010;
    let seed_stage1_token = match writer
        .try_claim_stage1_job(
            thread_ids[0],
            codex_protocol::ThreadId::new(),
            source_updated_at,
            /*lease_seconds*/ 60,
            /*max_running_jobs*/ 4,
        )
        .await?
    {
        Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
        outcome @ (Stage1JobClaimOutcome::SkippedUpToDate
        | Stage1JobClaimOutcome::SkippedRunning
        | Stage1JobClaimOutcome::SkippedRetryBackoff
        | Stage1JobClaimOutcome::SkippedRetryExhausted) => {
            anyhow::bail!("expected stage-one seed claim, got {outcome:?}")
        }
    };
    assert!(
        writer
            .mark_stage1_job_succeeded(
                thread_ids[0],
                &seed_stage1_token,
                source_updated_at,
                "concurrent reset raw memory",
                "concurrent reset summary",
                Some("concurrent-reset"),
            )
            .await?
    );
    let (phase2_token, input_watermark) = match writer
        .try_claim_global_phase2_job(codex_protocol::ThreadId::new(), /*lease_seconds*/ 60)
        .await?
    {
        Phase2JobClaimOutcome::Claimed {
            ownership_token,
            input_watermark,
        } => (ownership_token, input_watermark),
        outcome @ (Phase2JobClaimOutcome::SkippedRetryUnavailable
        | Phase2JobClaimOutcome::SkippedCooldown
        | Phase2JobClaimOutcome::SkippedRunning) => {
            anyhow::bail!("expected phase-two race claim, got {outcome:?}")
        }
    };
    let racing_stage1_token = match writer
        .try_claim_stage1_job(
            thread_ids[0],
            codex_protocol::ThreadId::new(),
            source_updated_at + 1,
            /*lease_seconds*/ 60,
            /*max_running_jobs*/ 4,
        )
        .await?
    {
        Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
        outcome @ (Stage1JobClaimOutcome::SkippedUpToDate
        | Stage1JobClaimOutcome::SkippedRunning
        | Stage1JobClaimOutcome::SkippedRetryBackoff
        | Stage1JobClaimOutcome::SkippedRetryExhausted) => {
            anyhow::bail!("expected stage-one race claim, got {outcome:?}")
        }
    };
    let racing_artifacts = MemoryArtifactSet::new(vec![MemoryArtifact::new(
        "MEMORY.md",
        b"concurrent generation\n".to_vec(),
    )?])?;
    let reset = resetter.clear_memory_data();
    let stage1 = writer.mark_stage1_job_succeeded(
        thread_ids[0],
        &racing_stage1_token,
        source_updated_at + 1,
        "racing raw memory",
        "racing rollout summary",
        Some("racing-reset"),
    );
    let phase2 = writer.complete_global_consolidation(
        &phase2_token,
        input_watermark,
        &[],
        &racing_artifacts,
    );
    let (reset_result, stage1_result, phase2_result) = tokio::join!(reset, stage1, phase2);
    reset_result?;
    let _ = (stage1_result?, phase2_result?);

    assert_eq!(
        postgres_snapshot(&setup_pool, fixture.schema()).await?,
        MemoryResetSnapshot {
            memory_jobs: 0,
            outputs: 0,
            used_outputs: 0,
            selected_outputs: 0,
            generations: 0,
            artifacts: 0,
            active_generations: 0,
            threads: 2,
            history_items: 3,
            disabled_modes: 1,
            pollution_overrides: 1,
        }
    );
    writer.close().await;
    resetter.close().await;
    setup_pool.close().await;
    fixture.cleanup().await
}

async fn postgres_snapshot(pool: &PgPool, schema: &str) -> Result<MemoryResetSnapshot> {
    let jobs = qualified_table(schema, "memory_jobs");
    let outputs = qualified_table(schema, "memory_stage1_outputs");
    let generations = qualified_table(schema, "memory_generations");
    let artifacts = qualified_table(schema, "memory_generation_artifacts");
    let generation_state = qualified_table(schema, "memory_generation_state");
    let threads = qualified_table(schema, "threads");
    let history = qualified_table(schema, "thread_history");
    let overrides = qualified_table(schema, "memory_thread_mode_overrides");
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT (SELECT COUNT(*) FROM {jobs} WHERE kind IN \
         ('memory_stage1', 'memory_consolidate_global')) AS memory_jobs, \
         (SELECT COUNT(*) FROM {outputs}) AS outputs, \
         (SELECT COUNT(*) FROM {outputs} WHERE usage_count > 0 AND last_usage IS NOT NULL) \
         AS used_outputs, (SELECT COUNT(*) FROM {outputs} WHERE selected_for_phase2 AND \
         selected_for_phase2_source_updated_at IS NOT NULL) AS selected_outputs, \
         (SELECT COUNT(*) FROM {generations}) AS generations, \
         (SELECT COUNT(*) FROM {artifacts}) AS artifacts, \
         (SELECT COUNT(*) FROM {generation_state} WHERE active_generation_id IS NOT NULL) \
         AS active_generations, (SELECT COUNT(*) FROM {threads}) AS threads, \
         (SELECT COUNT(*) FROM {history}) AS history_items, \
         (SELECT COUNT(*) FROM {history} WHERE item #>> '{{payload,memory_mode}}' = 'disabled') \
         AS disabled_modes, (SELECT COUNT(*) FROM {overrides}) AS pollution_overrides"
    )))
    .fetch_one(pool)
    .await?;
    Ok(MemoryResetSnapshot {
        memory_jobs: row.try_get("memory_jobs")?,
        outputs: row.try_get("outputs")?,
        used_outputs: row.try_get("used_outputs")?,
        selected_outputs: row.try_get("selected_outputs")?,
        generations: row.try_get("generations")?,
        artifacts: row.try_get("artifacts")?,
        active_generations: row.try_get("active_generations")?,
        threads: row.try_get("threads")?,
        history_items: row.try_get("history_items")?,
        disabled_modes: row.try_get("disabled_modes")?,
        pollution_overrides: row.try_get("pollution_overrides")?,
    })
}
