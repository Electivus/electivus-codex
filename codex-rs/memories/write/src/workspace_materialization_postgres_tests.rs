#![allow(
    clippy::disallowed_methods,
    reason = "PostgreSQL tests connect only to PostgreSQL pools"
)]

use super::prepare_memory_workspace_from_store;
use crate::collect_memory_artifacts;
use crate::reset_memories;
use anyhow::Context;
use anyhow::Result;
use codex_protocol::ThreadId;
use codex_state::ExternalAgentMemoryImport;
use codex_state::MemoryArtifact;
use codex_state::MemoryArtifactSet;
use codex_state::Phase2JobClaimOutcome;
use codex_state::PostgresNamespaceAction;
use codex_state::PostgresNamespaceConfig;
use codex_state::PostgresPoolConfig;
use codex_state::PostgresRuntimeStatePool;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;
use tempfile::TempDir;
use uuid::Uuid;

const DATABASE_URL_ENV: &str = "CODEX_TEST_POSTGRES_URL";

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_replica_materializes_generation_and_reset_clears_workspaces()
-> Result<()> {
    let database_url = std::env::var(DATABASE_URL_ENV)?;
    let schema = format!("codex_memory_materialize_{}", Uuid::new_v4().simple());
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
    let writer = writer_pool.memory_store();
    let reader = reader_pool.memory_store();

    let reader_home = TempDir::new()?;
    let reader_root = reader_home.path().join("memories");
    tokio::fs::create_dir(&reader_root).await?;
    tokio::fs::write(reader_root.join("stale.md"), "stale").await?;
    prepare_memory_workspace_from_store(&reader, &reader_root).await?;
    assert!(
        tokio::fs::read_dir(&reader_root)
            .await?
            .next_entry()
            .await?
            .is_none()
    );

    let writer_home = TempDir::new()?;
    let writer_root = writer_home.path().join("memories");
    tokio::fs::create_dir_all(writer_root.join("skills/example")).await?;
    tokio::fs::write(writer_root.join("MEMORY.md"), b"# shared memory\n").await?;
    let nested_bytes = b"skill\0contents\xff".to_vec();
    tokio::fs::write(writer_root.join("skills/example/SKILL.md"), &nested_bytes).await?;
    let artifacts = collect_memory_artifacts(&writer_root).await?;

    writer
        .enqueue_global_consolidation(/*input_watermark*/ 10)
        .await?;
    let (ownership_token, input_watermark) = match writer
        .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 60)
        .await?
    {
        Phase2JobClaimOutcome::Claimed {
            ownership_token,
            input_watermark,
        } => (ownership_token, input_watermark),
        outcome => anyhow::bail!("expected phase-two claim, got {outcome:?}"),
    };
    assert!(
        writer
            .complete_global_consolidation(&ownership_token, input_watermark, &[], &artifacts,)
            .await?
    );

    prepare_memory_workspace_from_store(&reader, &reader_root).await?;
    assert_ne!(writer_root, reader_root);
    assert_eq!(
        tokio::fs::read(reader_root.join("MEMORY.md")).await?,
        b"# shared memory\n"
    );
    assert_eq!(
        tokio::fs::read(reader_root.join("skills/example/SKILL.md")).await?,
        nested_bytes
    );

    let imported_resources = MemoryArtifactSet::new(vec![
        MemoryArtifact::new(
            "extensions/external_agent_import/instructions.md",
            b"# imported memory\n".to_vec(),
        )?,
        MemoryArtifact::new(
            "extensions/external_agent_import/resources/project-a/MEMORY.md",
            b"project memory\n".to_vec(),
        )?,
    ])?;
    writer_pool
        .external_agent_config_import_store()
        .record_completed_with_memory_import(
            "import-project-a",
            &[],
            &[],
            &ExternalAgentMemoryImport::new(vec!["project-a".to_string()], imported_resources)?,
        )
        .await?;
    prepare_memory_workspace_from_store(&reader, &reader_root).await?;
    assert_eq!(
        tokio::fs::read(
            reader_root.join("extensions/external_agent_import/resources/project-a/MEMORY.md")
        )
        .await?,
        b"project memory\n"
    );

    tokio::fs::create_dir(reader_root.join(".git")).await?;
    tokio::fs::write(reader_root.join(".git/HEAD"), "stale git metadata").await?;
    let extensions_root = reader_home.path().join("memories_extensions");
    tokio::fs::create_dir(&extensions_root).await?;
    tokio::fs::write(extensions_root.join("stale.md"), "stale extension").await?;
    reset_memories(&reader, reader_home.path()).await?;

    assert_eq!(
        reader.memory_workspace_materialization().await?,
        codex_state::MemoryWorkspaceMaterialization::Clear
    );
    assert!(
        tokio::fs::read_dir(&reader_root)
            .await?
            .next_entry()
            .await?
            .is_none()
    );
    assert!(
        tokio::fs::read_dir(&extensions_root)
            .await?
            .next_entry()
            .await?
            .is_none()
    );

    drop(writer);
    drop(reader);
    writer_pool.close().await;
    reader_pool.close().await;
    let cleanup_pool = sqlx::PgPool::connect(&database_url)
        .await
        .context("connect for PostgreSQL materialization test cleanup")?;
    sqlx::query(AssertSqlSafe(format!("DROP SCHEMA \"{schema}\" CASCADE")))
        .execute(&cleanup_pool)
        .await?;
    cleanup_pool.close().await;
    Ok(())
}

async fn mark_namespace_ready(database_url: &str, schema: &str) -> Result<()> {
    let pool = sqlx::PgPool::connect(database_url).await?;
    let migration = format!("\"{schema}\".runtime_state_migration");
    let evidence = serde_json::json!({
        "sourceIdentity": "memory-materialization-contract",
        "sourceFingerprint": "memory-materialization-contract-fingerprint",
        "phase": "ready",
        "ready": true,
        "fencingToken": 4,
        "namespaceDigest": "memory-materialization-contract-digest",
        "globalReferentialIntegrityValidated": true,
        "canonicalThreadHistoryOrderingValidated": true,
    });
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {migration} (source_identity, source_fingerprint, phase, ready, \
         phase_evidence, fencing_token) VALUES ($1, $2, 'ready', TRUE, $3, 4)"
    )))
    .bind("memory-materialization-contract")
    .bind("memory-materialization-contract-fingerprint")
    .bind(evidence)
    .execute(&pool)
    .await?;
    pool.close().await;
    Ok(())
}
