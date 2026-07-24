use super::PostgresNamespaceAction;
use super::qualified_table;
use super::test_support::PostgresContractFixture;
use super::test_support::test_database_url;
use crate::Phase2JobClaimOutcome;
use crate::Stage1JobClaimOutcome;
use crate::runtime::memory_store_phase2_success_contract_tests::persist_stage1_output;
use crate::runtime::memory_store_phase2_success_contract_tests::phase2_success_thread_ids;
use crate::runtime::memory_store_phase2_success_contract_tests::seed_postgres_phase2_success_threads;
use anyhow::Context;
use anyhow::Result;
use chrono::Utc;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

async fn wait_until_postgres_backend_is_blocked(pool: &PgPool, backend_pid: i32) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let is_blocked: bool =
                sqlx::query_scalar("SELECT cardinality(pg_blocking_pids($1)) > 0")
                    .bind(backend_pid)
                    .fetch_one(pool)
                    .await?;
            if is_blocked {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("PostgreSQL backend did not block on the coordinated row lock")?
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_stage1_refresh_and_phase2_success_do_not_deadlock() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url.clone(), "memory_lock_order")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let phase2_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await?;
    let stage1_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await?;
    let blocker_pool = fixture.connect_pool().await?;
    let observer_pool = fixture.connect_pool().await?;
    let thread_ids = phase2_success_thread_ids()?;
    seed_postgres_phase2_success_threads(&phase2_pool, fixture.schema(), thread_ids).await?;
    let phase2 =
        crate::MemoryStore::from_postgres(phase2_pool.clone(), fixture.schema().to_string());
    let stage1 =
        crate::MemoryStore::from_postgres(stage1_pool.clone(), fixture.schema().to_string());
    phase2.clear_memory_data().await?;

    let source_updated_at = Utc::now().timestamp().saturating_sub(60 * 60);
    persist_stage1_output(
        &phase2,
        thread_ids[0],
        source_updated_at,
        "before-lock-order-race",
    )
    .await?;
    let selected_outputs = phase2
        .get_phase2_input_selection(/*n*/ 1, /*max_unused_days*/ 36_500)
        .await?;
    let phase2_token = match phase2
        .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
        .await?
    {
        Phase2JobClaimOutcome::Claimed {
            ownership_token,
            input_watermark,
        } => {
            assert_eq!(input_watermark, source_updated_at);
            ownership_token
        }
        outcome => anyhow::bail!("expected phase-two claim, got {outcome:?}"),
    };
    let replacement_source_updated_at = source_updated_at + 1;
    let stage1_token = match stage1
        .try_claim_stage1_job(
            thread_ids[0],
            ThreadId::new(),
            replacement_source_updated_at,
            /*lease_seconds*/ 60,
            /*max_running_jobs*/ 4,
        )
        .await?
    {
        Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
        outcome => anyhow::bail!("expected replacement stage-one claim, got {outcome:?}"),
    };

    let jobs_table = qualified_table(fixture.schema(), "memory_jobs");
    let mut blocker = blocker_pool.begin().await?;
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await?;
    sqlx::query(AssertSqlSafe(format!(
        "SELECT 1 FROM {jobs_table} WHERE kind = 'memory_consolidate_global' \
         AND job_key = 'global' FOR UPDATE"
    )))
    .fetch_one(&mut *blocker)
    .await?;
    let phase2_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&phase2_pool)
        .await?;
    let stage1_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&stage1_pool)
        .await?;

    let phase2_store = phase2.clone();
    let phase2_task = tokio::spawn(async move {
        phase2_store
            .mark_global_phase2_job_succeeded(&phase2_token, source_updated_at, &selected_outputs)
            .await
    });
    wait_until_postgres_backend_is_blocked(&observer_pool, phase2_pid).await?;

    let stage1_store = stage1.clone();
    let stage1_task = tokio::spawn(async move {
        stage1_store
            .mark_stage1_job_succeeded(
                thread_ids[0],
                &stage1_token,
                replacement_source_updated_at,
                "raw memory after lock-order race",
                "rollout summary after lock-order race",
                Some("after-lock-order-race"),
            )
            .await
    });
    wait_until_postgres_backend_is_blocked(&observer_pool, stage1_pid).await?;
    assert_ne!(blocker_pid, phase2_pid);
    assert_ne!(blocker_pid, stage1_pid);
    blocker.commit().await?;

    let (phase2_result, stage1_result) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(phase2_task, stage1_task)
    })
    .await
    .context("memory mutations did not finish after releasing the coordinating row lock")?;
    assert_eq!(
        (
            phase2_result.context("join phase-two success task")??,
            stage1_result.context("join stage-one refresh task")??,
        ),
        (true, true)
    );
    let phase2_outputs = phase2.list_stage1_outputs_for_global(/*n*/ 10).await?;
    assert_eq!(
        stage1.list_stage1_outputs_for_global(/*n*/ 10).await?,
        phase2_outputs
    );
    assert_eq!(phase2_outputs.len(), 1);
    assert_eq!(
        phase2_outputs[0].source_updated_at.timestamp(),
        replacement_source_updated_at
    );
    assert_eq!(
        phase2
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
            .await?,
        Phase2JobClaimOutcome::SkippedCooldown
    );

    phase2.close().await;
    stage1.close().await;
    blocker_pool.close().await;
    observer_pool.close().await;
    fixture.cleanup().await
}
