use super::MigrationHistory;
use super::PostgresNamespaceAction;
use super::test_support::PostgresContractFixture;
use super::test_support::test_database_url;
use crate::runtime::LogStore;
use crate::runtime::logs_contract_tests::run_feedback_contract;
use crate::runtime::logs_contract_tests::run_filter_order_and_max_id_contract;
use crate::runtime::logs_contract_tests::run_partition_limits_contract;
use crate::runtime::logs_contract_tests::run_replica_visibility_contract;
use crate::runtime::logs_contract_tests::run_startup_retention_contract;
use anyhow::Context;
use anyhow::Result;
use pretty_assertions::assert_eq;
use std::time::Duration;

async fn postgres_log_replicas(fixture: &PostgresContractFixture) -> Result<(LogStore, LogStore)> {
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let writer =
        LogStore::from_postgres(fixture.connect_pool().await?, fixture.schema().to_string());
    let reader =
        LogStore::from_postgres(fixture.connect_pool().await?, fixture.schema().to_string());
    Ok((writer, reader))
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_logs_share_single_and_batch_inserts_between_replicas() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "logs_visibility")?;
    let (writer, reader) = postgres_log_replicas(&fixture).await?;

    run_replica_visibility_contract(&writer, &reader).await?;

    writer.close().await;
    reader.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_logs_match_filters_order_and_maximum_id() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "logs_filters")?;
    let (writer, reader) = postgres_log_replicas(&fixture).await?;

    run_filter_order_and_max_id_contract(&writer, &reader).await?;

    writer.close().await;
    reader.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_logs_render_feedback_bodies() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "logs_feedback")?;
    let (writer, reader) = postgres_log_replicas(&fixture).await?;

    run_feedback_contract(&writer, &reader).await?;

    writer.close().await;
    reader.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_logs_apply_startup_retention() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "logs_startup_retention")?;
    let (writer, reader) = postgres_log_replicas(&fixture).await?;

    run_startup_retention_contract(&writer, &reader).await?;

    writer.close().await;
    reader.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_logs_preserve_partition_limits() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "logs_partition_limits")?;
    let (writer, reader) = postgres_log_replicas(&fixture).await?;

    run_partition_limits_contract(&writer, &reader).await?;

    writer.close().await;
    reader.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_log_failures_are_sanitized_without_sqlite_fallback() -> Result<()> {
    let database_url = test_database_url()?;
    let secret = database_url.clone();
    let mut fixture = PostgresContractFixture::new(database_url, "logs_failure")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let store =
        LogStore::from_postgres(fixture.connect_pool().await?, fixture.schema().to_string());
    store.close().await;

    let error = store
        .query_logs(&crate::LogQuery::default())
        .await
        .expect_err("a closed PostgreSQL pool must fail without fallback");
    let message = error.to_string();
    let rendered = format!("{error:?} {error}");
    assert_eq!(
        message,
        format!(
            "PostgreSQL could not complete the `query logs` operation for schema `{}`; verify the namespace and server health, then retry",
            fixture.schema()
        )
    );
    assert!(!rendered.contains(&secret));
    assert!(!rendered.contains("SELECT "));
    assert!(!message.to_ascii_lowercase().contains("sqlite"));

    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_creates_migrates_validates_and_cleans_up_namespace() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "lifecycle")?;

    assert!(!fixture.schema_exists().await?);
    let migrated = fixture.manage(PostgresNamespaceAction::Migrate).await?;
    assert!(fixture.schema_exists().await?);
    let validated = fixture.manage(PostgresNamespaceAction::Validate).await?;

    assert_eq!(validated, migrated);
    assert_eq!(migrated.schema(), fixture.schema());
    assert_eq!(migrated.version(), 2);

    fixture.cleanup().await?;
    assert!(!fixture.schema_exists().await?);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_migration_is_idempotent() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "idempotent")?;

    let first = fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let second = fixture.manage(PostgresNamespaceAction::Migrate).await?;

    assert_eq!(second, first);
    assert_eq!(
        fixture.migration_history().await?,
        MigrationHistory {
            minimum: Some(1),
            maximum: Some(2),
            count: 2,
        }
    );
    fixture.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_validation_is_read_only() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "readonly")?;

    let missing_error = fixture
        .validate_read_only()
        .await
        .expect_err("validation should not create an absent schema");
    assert!(missing_error.to_string().contains("does not exist"));
    assert!(!fixture.schema_exists().await?);

    let migrated = fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let history_before = fixture.migration_history().await?;
    let validated = fixture.validate_read_only().await?;
    let history_after = fixture.migration_history().await?;

    assert_eq!(validated, migrated);
    assert_eq!(history_after, history_before);

    fixture.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_migration_uses_namespace_advisory_lock() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "locking")?;
    let initial = fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let mut held_lock = fixture.hold_migration_lock().await?;
    let mut contending_migration = Box::pin(fixture.manage(PostgresNamespaceAction::Migrate));

    tokio::select! {
        result = &mut contending_migration => {
            anyhow::bail!(
                "migration completed before the namespace advisory lock was released: {result:?}"
            );
        }
        observed = held_lock.wait_for_waiter() => {
            observed.context("observe migration waiting on the namespace advisory lock")?;
        }
    }

    held_lock.release().await?;
    let contending = tokio::time::timeout(Duration::from_secs(5), &mut contending_migration)
        .await
        .context("migration did not resume after the namespace advisory lock was released")??;
    assert_eq!(contending, initial);
    assert_eq!(
        fixture.migration_history().await?,
        MigrationHistory {
            minimum: Some(1),
            maximum: Some(2),
            count: 2,
        }
    );
    drop(contending_migration);

    fixture.cleanup().await?;
    Ok(())
}
