use super::PostgresRuntimeStatePool;
use super::test_support::PostgresContractFixture;
use super::test_support::test_database_url;
use crate::initialize_postgres_runtime_state;
use anyhow::Context;
use anyhow::Result;
use pretty_assertions::assert_eq;

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_initializes_an_empty_runtime_ready_namespace() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "initialize_empty")?;

    let report = initialize_postgres_runtime_state(fixture.config_for_tests()).await?;

    assert_eq!(report.schema(), fixture.schema());
    assert_eq!(report.fencing_token(), 1);
    assert_eq!(
        report.evidence()["initializationMode"],
        serde_json::Value::String("empty".to_string())
    );

    let runtime_pool = PostgresRuntimeStatePool::connect(fixture.config_for_tests()).await?;
    let generation = runtime_pool
        .memory_store()
        .load_active_memory_generation()
        .await?
        .context("empty initialization must publish an active Memory Generation")?;
    assert_eq!(generation.completed_watermark(), 0);
    assert_eq!(generation.artifacts(), []);
    runtime_pool.close().await;

    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_empty_initialization_never_resets_a_ready_namespace() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "initialize_no_reset")?;
    initialize_postgres_runtime_state(fixture.config_for_tests()).await?;
    let runtime_pool = PostgresRuntimeStatePool::connect(fixture.config_for_tests()).await?;
    let generation_before = runtime_pool
        .memory_store()
        .load_active_memory_generation()
        .await?
        .context("initialized namespace must have an active Memory Generation")?;
    runtime_pool.close().await;

    let error = initialize_postgres_runtime_state(fixture.config_for_tests())
        .await
        .expect_err("initialization must reject an already-ready namespace");

    assert!(
        error
            .to_string()
            .contains("contains non-baseline coordination state")
    );
    let runtime_pool = PostgresRuntimeStatePool::connect(fixture.config_for_tests()).await?;
    let generation_after = runtime_pool
        .memory_store()
        .load_active_memory_generation()
        .await?
        .context("rejected initialization must preserve the active Memory Generation")?;
    assert_eq!(generation_after, generation_before);
    runtime_pool.close().await;

    fixture.cleanup().await
}
