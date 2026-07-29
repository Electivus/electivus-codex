use super::PostgresNamespaceAction;
use super::test_support::PostgresContractFixture;
use super::test_support::test_database_url;
use pretty_assertions::assert_eq;

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_namespace_migrations_require_no_extensions() -> anyhow::Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "no_extensions")?;
    let before = installed_extensions(&fixture).await?;

    fixture.manage(PostgresNamespaceAction::Migrate).await?;

    assert_eq!(installed_extensions(&fixture).await?, before);
    let pool = fixture.connect_pool().await?;
    let routines = sqlx::query_scalar::<_, String>(
        "SELECT routine_name FROM information_schema.routines \
         WHERE routine_schema = $1 ORDER BY routine_name",
    )
    .bind(fixture.schema())
    .fetch_all(&pool)
    .await?;
    assert_eq!(routines, Vec::<String>::new());
    pool.close().await;
    fixture.cleanup().await
}

async fn installed_extensions(fixture: &PostgresContractFixture) -> anyhow::Result<Vec<String>> {
    let pool = fixture.connect_pool().await?;
    let extensions = sqlx::query_scalar("SELECT extname FROM pg_extension ORDER BY extname")
        .fetch_all(&pool)
        .await?;
    pool.close().await;
    Ok(extensions)
}
