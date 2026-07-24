use super::PostgresNamespaceAction;
use super::qualified_table;
use super::test_support::PostgresContractFixture;
use super::test_support::test_database_url;
use crate::Phase2JobClaimOutcome;
use crate::runtime::MemoryArtifact;
use crate::runtime::MemoryArtifactSet;
use anyhow::Context;
use anyhow::Result;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;

fn artifacts(version: &str) -> MemoryArtifactSet {
    MemoryArtifactSet::new(vec![
        MemoryArtifact::new("MEMORY.md", format!("# {version} memory\n").into_bytes())
            .expect("valid memory artifact"),
        MemoryArtifact::new(
            "memory_summary.md",
            format!("v1\n\n{version} summary\n").into_bytes(),
        )
        .expect("valid memory summary artifact"),
        MemoryArtifact::new(
            "skills/example/SKILL.md",
            format!("# {version} skill\n").into_bytes(),
        )
        .expect("valid skill artifact"),
    ])
    .expect("valid memory artifact set")
}

async fn claim(store: &crate::MemoryStore, input_watermark: i64) -> Result<(String, i64)> {
    store.enqueue_global_consolidation(input_watermark).await?;
    match store
        .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
        .await?
    {
        Phase2JobClaimOutcome::Claimed {
            ownership_token,
            input_watermark,
        } => Ok((ownership_token, input_watermark)),
        outcome => anyhow::bail!("expected phase-two claim, got {outcome:?}"),
    }
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_publishes_complete_fenced_memory_generations() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "memory_generation")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let setup_pool = fixture.connect_pool().await?;
    let writer = crate::MemoryStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    let reader = crate::MemoryStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    assert_eq!(reader.load_active_memory_generation().await?, None);

    let (first_token, first_watermark) = claim(&writer, /*input_watermark*/ 10).await?;
    let first_artifacts = artifacts("first");
    assert!(
        writer
            .complete_global_consolidation(&first_token, first_watermark, &[], &first_artifacts,)
            .await?
    );
    let first_generation = reader
        .load_active_memory_generation()
        .await?
        .context("current owner should publish the first generation")?;
    assert_eq!(
        reader.load_active_memory_generation().await?,
        Some(first_generation.clone())
    );

    let jobs_table = qualified_table(fixture.schema(), "memory_jobs");
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {jobs_table} SET finished_at = 0 WHERE kind = 'memory_consolidate_global' \
         AND job_key = 'global'"
    )))
    .execute(&setup_pool)
    .await?;
    let (second_token, second_watermark) = claim(&writer, /*input_watermark*/ 20).await?;
    let second_artifacts = artifacts("second");
    let writer_for_publish = writer.clone();
    let second_artifacts_for_publish = second_artifacts.clone();
    let publish = tokio::spawn(async move {
        writer_for_publish
            .complete_global_consolidation(
                &second_token,
                second_watermark,
                &[],
                &second_artifacts_for_publish,
            )
            .await
    });

    while !publish.is_finished() {
        let observed = reader
            .load_active_memory_generation()
            .await?
            .context("the first generation should remain readable")?;
        assert!(
            observed.artifacts() == first_generation.artifacts()
                || observed.artifacts() == second_artifacts.artifacts(),
            "reader observed a cross-generation artifact mixture: {observed:?}"
        );
    }
    assert!(
        publish
            .await
            .context("join second generation publication")??
    );
    let second_generation = reader
        .load_active_memory_generation()
        .await?
        .context("current owner should publish the second generation")?;
    assert_eq!(
        reader.load_active_memory_generation().await?,
        Some(second_generation.clone())
    );

    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {jobs_table} SET finished_at = 0 WHERE kind = 'memory_consolidate_global' \
         AND job_key = 'global'"
    )))
    .execute(&setup_pool)
    .await?;
    let (stale_token, _) = claim(&writer, /*input_watermark*/ 30).await?;
    assert!(
        writer
            .heartbeat_global_phase2_job(&stale_token, /*lease_seconds*/ 0)
            .await?
    );
    let takeover = reader
        .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
        .await?;
    assert!(matches!(takeover, Phase2JobClaimOutcome::Claimed { .. }));

    assert_eq!(
        writer
            .complete_global_consolidation(
                &stale_token,
                /*completed_watermark*/ 30,
                &[],
                &artifacts("stale")
            )
            .await?,
        false
    );
    assert_eq!(
        reader.load_active_memory_generation().await?,
        Some(second_generation)
    );

    writer.close().await;
    reader.close().await;
    setup_pool.close().await;
    fixture.cleanup().await
}
