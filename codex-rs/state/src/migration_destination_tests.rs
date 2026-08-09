use super::destination_validation;
use super::preflight_runtime_state_migration;
use super::test_support;
use crate::PostgresNamespaceAction;
use crate::SqliteConfig;
use crate::postgres::MAXIMUM_COMPATIBLE_SCHEMA_VERSION;
use crate::postgres::qualified_table;
use crate::postgres::quote_identifier;
use crate::postgres::test_support::PostgresContractFixture;
use crate::postgres::test_support::test_database_url;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;
use sqlx::Connection;

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_destination_emptiness_accepts_only_builtin_thread_section()
-> anyhow::Result<()> {
    let database_url = test_database_url()?;
    let mut destination = PostgresContractFixture::new(database_url, "empty_sections")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;
    let pool = destination.connect_pool().await?;
    let mut connection = pool.acquire().await?;

    destination_validation::ensure_empty(&mut connection, destination.schema()).await?;

    let sections = qualified_table(destination.schema(), "thread_sections");
    let non_baseline_cases = [
        (
            "an additional custom thread section",
            format!("INSERT INTO {sections} (id, name) VALUES ('custom-section', 'Custom')"),
        ),
        (
            "a missing built-in thread section",
            format!("DELETE FROM {sections}"),
        ),
        (
            "a replacement custom thread section",
            format!("UPDATE {sections} SET id = 'custom-section'"),
        ),
        (
            "a renamed built-in thread section",
            format!("UPDATE {sections} SET name = 'Renamed'"),
        ),
    ];
    for (case, mutation) in non_baseline_cases {
        let mut transaction = connection.begin().await?;
        sqlx::query(AssertSqlSafe(mutation))
            .execute(transaction.as_mut())
            .await?;
        let error =
            destination_validation::ensure_empty(transaction.as_mut(), destination.schema())
                .await
                .expect_err(case);
        assert!(
            error
                .to_string()
                .contains("non-baseline coordination state"),
            "{case}: {error:#}"
        );
        transaction.rollback().await?;
    }
    drop(connection);
    pool.close().await;
    destination.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_preflight_rejects_nonempty_destination_without_writes()
-> anyhow::Result<()> {
    let source = test_support::initialized_source("destination-nonempty").await?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let database_url = test_database_url()?;
    let mut destination = PostgresContractFixture::new(database_url.clone(), "preflight_nonempty")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;
    let pool = destination.connect_pool().await?;
    let logs = qualified_table(destination.schema(), "logs");
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {logs} (ts, ts_nanos, level, target) VALUES (1, 2, 'INFO', 'fixture')"
    )))
    .execute(&pool)
    .await?;
    pool.close().await;
    let source_before = test_support::snapshot_source(&source)?;
    let destination_before = test_support::snapshot_destination(&destination).await?;

    let error = preflight_runtime_state_migration(
        SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?),
        destination.config_for_tests(),
    )
    .await
    .expect_err("nonempty destination must be rejected");

    let rendered = format!("{error:?} {error:#}");
    assert!(rendered.contains("not empty"), "{rendered}");
    assert!(!rendered.contains(&database_url));
    assert_eq!(test_support::snapshot_source(&source)?, source_before);
    assert_eq!(
        test_support::snapshot_destination(&destination).await?,
        destination_before
    );

    destination.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_preflight_rejects_incompatible_version_without_writes()
-> anyhow::Result<()> {
    let source = test_support::initialized_source("destination-version").await?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let database_url = test_database_url()?;
    let mut destination = PostgresContractFixture::new(database_url.clone(), "preflight_version")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;
    let pool = destination.connect_pool().await?;
    let migrations = qualified_table(destination.schema(), "_codex_runtime_state_migrations");
    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {migrations} WHERE version = $1"
    )))
    .bind(MAXIMUM_COMPATIBLE_SCHEMA_VERSION)
    .execute(&pool)
    .await?;
    pool.close().await;
    let source_before = test_support::snapshot_source(&source)?;
    let destination_before = test_support::snapshot_destination(&destination).await?;

    let error = preflight_runtime_state_migration(
        SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?),
        destination.config_for_tests(),
    )
    .await
    .expect_err("outdated destination must be rejected");

    let rendered = format!("{error:?} {error:#}");
    assert!(
        rendered.contains(&format!(
            "current version {MAXIMUM_COMPATIBLE_SCHEMA_VERSION}"
        )),
        "{rendered}"
    );
    assert!(!rendered.contains(&database_url));
    assert_eq!(test_support::snapshot_source(&source)?, source_before);
    assert_eq!(
        test_support::snapshot_destination(&destination).await?,
        destination_before
    );
    let pool = destination.connect_pool().await?;
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {migrations} (version) VALUES ($1)"
    )))
    .bind(MAXIMUM_COMPATIBLE_SCHEMA_VERSION)
    .execute(&pool)
    .await?;
    let logs = qualified_table(destination.schema(), "logs");
    sqlx::query(AssertSqlSafe(format!("DROP TABLE {logs}")))
        .execute(&pool)
        .await?;
    pool.close().await;
    let source_before = test_support::snapshot_source(&source)?;
    let destination_before = test_support::snapshot_destination(&destination).await?;
    let error = preflight_runtime_state_migration(
        SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?),
        destination.config_for_tests(),
    )
    .await
    .expect_err("incompatible destination layout must be rejected");
    assert!(
        error.to_string().contains("incompatible table layout"),
        "{error:#}"
    );
    assert_eq!(test_support::snapshot_source(&source)?, source_before);
    assert_eq!(
        test_support::snapshot_destination(&destination).await?,
        destination_before
    );
    destination.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_preflight_rejects_unexpected_functions_and_triggers()
-> anyhow::Result<()> {
    let source = test_support::initialized_source("destination-trigger").await?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let database_url = test_database_url()?;
    let mut destination = PostgresContractFixture::new(database_url, "preflight_trigger")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;
    let pool = destination.connect_pool().await?;
    let schema = quote_identifier(destination.schema());
    let logs = qualified_table(destination.schema(), "logs");
    sqlx::query(AssertSqlSafe(format!(
        "CREATE FUNCTION {schema}.unexpected_function() RETURNS integer LANGUAGE SQL \
         AS 'SELECT 1'"
    )))
    .execute(&pool)
    .await?;
    sqlx::query(AssertSqlSafe(format!(
        "CREATE TRIGGER unexpected_trigger BEFORE UPDATE ON {logs} FOR EACH ROW \
         EXECUTE FUNCTION pg_catalog.suppress_redundant_updates_trigger()"
    )))
    .execute(&pool)
    .await?;
    pool.close().await;

    let error = preflight_runtime_state_migration(
        SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?),
        destination.config_for_tests(),
    )
    .await
    .expect_err("unexpected executable schema objects must be rejected");

    assert!(
        error.to_string().contains("incompatible table layout"),
        "{error:#}"
    );
    destination.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_preflight_does_not_create_an_absent_destination_schema()
-> anyhow::Result<()> {
    let source = test_support::initialized_source("destination-absent").await?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let database_url = test_database_url()?;
    let mut destination = PostgresContractFixture::new(database_url.clone(), "preflight_absent")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;
    let pool = destination.connect_pool().await?;
    let schema = quote_identifier(destination.schema());
    sqlx::query(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&pool)
        .await?;
    pool.close().await;
    let source_before = test_support::snapshot_source(&source)?;

    let error = preflight_runtime_state_migration(
        SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?),
        destination.config_for_tests(),
    )
    .await
    .expect_err("absent destination schema must not be created");

    let rendered = format!("{error:?} {error:#}");
    assert!(rendered.contains("does not exist"), "{rendered}");
    assert!(!rendered.contains(&database_url));
    assert_eq!(test_support::snapshot_source(&source)?, source_before);
    assert!(!destination.schema_exists().await?);
    destination.cleanup().await?;
    Ok(())
}
