use super::MigrationHistory;
use super::PostgresNamespaceAction;
use super::test_support::PostgresContractFixture;
use super::test_support::test_database_url;
use anyhow::Context;
use anyhow::Result;
use pretty_assertions::assert_eq;
use std::time::Duration;

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
    assert_eq!(migrated.version(), 1);

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
            maximum: Some(1),
            count: 1,
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
            maximum: Some(1),
            count: 1,
        }
    );
    drop(contending_migration);

    fixture.cleanup().await?;
    Ok(())
}
