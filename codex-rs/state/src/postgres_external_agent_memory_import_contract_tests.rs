use super::PostgresNamespaceAction;
use super::test_support::PostgresContractFixture;
use super::test_support::test_database_url;
use crate::Phase2JobClaimOutcome;
use anyhow::Context;
use anyhow::Result;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

fn imported_memory(
    project_key: &str,
    artifact_name: &str,
    contents: &[u8],
) -> Result<crate::ExternalAgentMemoryImport> {
    crate::ExternalAgentMemoryImport::new(
        vec![project_key.to_string()],
        crate::MemoryArtifactSet::new(vec![
            crate::MemoryArtifact::new(
                "extensions/external_agent_import/instructions.md",
                b"# imported memory rules".to_vec(),
            )?,
            crate::MemoryArtifact::new(
                format!("extensions/external_agent_import/resources/{project_key}/{artifact_name}"),
                contents.to_vec(),
            )?,
        ])?,
    )
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_external_agent_memory_import_is_atomic_and_idempotent() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "external_agent_memory")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let writer = crate::ExternalAgentConfigImportStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    let second_writer = crate::ExternalAgentConfigImportStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    let memory_reader = crate::MemoryStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    let successes = [crate::ExternalAgentConfigImportSuccessRecord {
        item_type: "MEMORY".to_string(),
        cwd: None,
        source: Some("project-a".to_string()),
        target: Some("memories/extensions/external_agent_import/resources".to_string()),
    }];
    let project_a = imported_memory("project-a", "MEMORY.md", b"alpha")?;

    writer
        .record_completed_with_memory_import("import-a", &successes, &[], &project_a)
        .await?;
    let first_generation = memory_reader
        .load_active_memory_generation()
        .await?
        .context("memory import should publish an active generation")?;
    assert_eq!(
        first_generation.artifacts(),
        project_a.artifacts().artifacts()
    );
    assert_eq!(
        second_writer.details("import-a").await?,
        Some(crate::ExternalAgentConfigImportDetailsRecord {
            successes: successes.to_vec(),
            failures: vec![],
        })
    );

    writer
        .record_completed_with_memory_import("import-a", &successes, &[], &project_a)
        .await?;
    assert_eq!(
        memory_reader
            .load_active_memory_generation()
            .await?
            .context("retry should retain the active generation")?
            .generation_id(),
        first_generation.generation_id()
    );

    let conflicting_retry = imported_memory("project-a", "MEMORY.md", b"conflict")?;
    assert!(
        writer
            .record_completed_with_memory_import("import-a", &successes, &[], &conflicting_retry,)
            .await
            .is_err()
    );
    assert_eq!(
        memory_reader.load_active_memory_generation().await?,
        Some(first_generation)
    );

    let project_b = imported_memory("project-b", "topic.md", b"bravo")?;
    second_writer
        .record_completed_with_memory_import("import-b", &successes, &[], &project_b)
        .await?;
    let merged = memory_reader
        .load_active_memory_generation()
        .await?
        .context("second import should publish a merged generation")?;
    assert_eq!(
        merged
            .artifacts()
            .iter()
            .map(|artifact| (artifact.path(), artifact.contents()))
            .collect::<Vec<_>>(),
        vec![
            (
                "extensions/external_agent_import/instructions.md",
                b"# imported memory rules".as_slice(),
            ),
            (
                "extensions/external_agent_import/resources/project-a/MEMORY.md",
                b"alpha".as_slice(),
            ),
            (
                "extensions/external_agent_import/resources/project-b/topic.md",
                b"bravo".as_slice(),
            ),
        ]
    );

    let remove_project_a = crate::ExternalAgentMemoryImport::new(
        vec!["project-a".to_string()],
        crate::MemoryArtifactSet::new(Vec::new())?,
    )?;
    second_writer
        .record_completed_with_memory_import("import-c", &successes, &[], &remove_project_a)
        .await?;
    assert_eq!(
        memory_reader
            .load_active_memory_generation()
            .await?
            .context("selected project removal should publish a generation")?
            .artifacts()
            .iter()
            .map(crate::MemoryArtifact::path)
            .collect::<Vec<_>>(),
        vec![
            "extensions/external_agent_import/instructions.md",
            "extensions/external_agent_import/resources/project-b/topic.md",
        ]
    );

    let claim = memory_reader
        .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
        .await?;
    let Phase2JobClaimOutcome::Claimed {
        ownership_token,
        input_watermark,
    } = claim
    else {
        anyhow::bail!("expected imported resources to enqueue consolidation, got {claim:?}");
    };
    let stale_workspace = crate::MemoryArtifactSet::new(vec![crate::MemoryArtifact::new(
        "MEMORY.md",
        b"consolidated".to_vec(),
    )?])?;
    assert!(
        memory_reader
            .complete_global_consolidation(
                &ownership_token,
                input_watermark,
                &[],
                &stale_workspace,
            )
            .await?
    );
    assert_eq!(
        memory_reader
            .load_active_memory_generation()
            .await?
            .context("consolidation should publish a generation")?
            .artifacts()
            .iter()
            .map(crate::MemoryArtifact::path)
            .collect::<Vec<_>>(),
        vec![
            "MEMORY.md",
            "extensions/external_agent_import/instructions.md",
            "extensions/external_agent_import/resources/project-b/topic.md",
        ]
    );

    memory_reader.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_concurrent_memory_imports_merge_at_generation_boundary() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "concurrent_imports")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let first = crate::ExternalAgentConfigImportStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    let second = crate::ExternalAgentConfigImportStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    let reader = crate::MemoryStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    let first_import = imported_memory("project-a", "MEMORY.md", b"alpha")?;
    let second_import = imported_memory("project-b", "MEMORY.md", b"bravo")?;

    let (first_result, second_result) = tokio::join!(
        first.record_completed_with_memory_import("concurrent-a", &[], &[], &first_import),
        second.record_completed_with_memory_import("concurrent-b", &[], &[], &second_import),
    );
    first_result?;
    second_result?;

    assert_eq!(
        reader
            .load_active_memory_generation()
            .await?
            .context("concurrent imports should publish one complete active generation")?
            .artifacts()
            .iter()
            .map(crate::MemoryArtifact::path)
            .collect::<Vec<_>>(),
        vec![
            "extensions/external_agent_import/instructions.md",
            "extensions/external_agent_import/resources/project-a/MEMORY.md",
            "extensions/external_agent_import/resources/project-b/MEMORY.md",
        ]
    );
    assert_eq!(first.history().await?.len(), 2);

    reader.close().await;
    fixture.cleanup().await
}
