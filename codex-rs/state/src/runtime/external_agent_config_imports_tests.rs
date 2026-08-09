use super::*;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

#[test]
fn external_agent_memory_import_validates_replacement_scope() -> anyhow::Result<()> {
    let removal = ExternalAgentMemoryImport::new(
        vec!["project-a".to_string()],
        MemoryArtifactSet::new(Vec::new())?,
    )?;
    assert_eq!(removal.project_keys(), &["project-a".to_string()]);
    assert_eq!(removal.artifacts(), &MemoryArtifactSet::new(Vec::new())?);

    let outside_scope = MemoryArtifactSet::new(vec![MemoryArtifact::new(
        "extensions/external_agent_import/resources/project-b/MEMORY.md",
        Vec::new(),
    )?])?;
    assert!(ExternalAgentMemoryImport::new(vec!["project-a".to_string()], outside_scope).is_err());
    assert!(
        ExternalAgentMemoryImport::new(
            vec!["..".to_string()],
            MemoryArtifactSet::new(Vec::new())?,
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn external_agent_memory_import_overlay_applies_later_project_replacements() -> anyhow::Result<()> {
    let first = ExternalAgentMemoryImport::new(
        vec!["project-a".to_string()],
        MemoryArtifactSet::new(vec![
            MemoryArtifact::new(IMPORTED_MEMORY_INSTRUCTIONS_PATH, b"rules".to_vec())?,
            MemoryArtifact::new(
                "extensions/external_agent_import/resources/project-a/MEMORY.md",
                b"alpha".to_vec(),
            )?,
        ])?,
    )?;
    let second = ExternalAgentMemoryImport::new(
        vec!["project-b".to_string()],
        MemoryArtifactSet::new(vec![MemoryArtifact::new(
            "extensions/external_agent_import/resources/project-b/MEMORY.md",
            b"bravo".to_vec(),
        )?])?,
    )?;
    let remove_first = ExternalAgentMemoryImport::new(
        vec!["project-a".to_string()],
        MemoryArtifactSet::new(Vec::new())?,
    )?;

    let merged = first.overlay(second)?.overlay(remove_first)?;
    assert_eq!(
        merged
            .artifacts()
            .artifacts()
            .iter()
            .map(MemoryArtifact::path)
            .collect::<Vec<_>>(),
        vec![
            IMPORTED_MEMORY_INSTRUCTIONS_PATH,
            "extensions/external_agent_import/resources/project-b/MEMORY.md",
        ]
    );
    assert_eq!(
        merged.project_keys(),
        &["project-a".to_string(), "project-b".to_string()]
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_matches_external_agent_config_import_contract() -> anyhow::Result<()> {
    let runtime = StateRuntime::init_sqlite(unique_temp_dir(), "test-provider".to_string()).await?;

    crate::runtime::external_agent_config_imports_contract_tests::run_external_agent_config_import_contract(
        &runtime.external_agent_config_imports,
        &runtime.external_agent_config_imports,
    )
    .await
}

#[tokio::test]
async fn records_completion_by_import_id() -> anyhow::Result<()> {
    let runtime = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(unique_temp_dir().as_path().abs()),
        "test-provider".to_string(),
    )
    .await?;

    runtime
        .record_external_agent_config_import_completed(
            "import-1",
            Some("provider-1"),
            &[ExternalAgentConfigImportSuccessRecord {
                item_type: "CONFIG".to_string(),
                cwd: None,
                source: Some("settings.json".to_string()),
                target: Some("config.toml".to_string()),
                title: None,
            }],
            &[],
        )
        .await?;
    runtime
        .record_external_agent_config_import_completed(
            "import-1",
            Some("provider-2"),
            &[
                ExternalAgentConfigImportSuccessRecord {
                    item_type: "CONFIG".to_string(),
                    cwd: None,
                    source: Some("settings.json".to_string()),
                    target: Some("config.toml".to_string()),
                    title: None,
                },
                ExternalAgentConfigImportSuccessRecord {
                    item_type: "MCP_SERVER_CONFIG".to_string(),
                    cwd: None,
                    source: Some("github".to_string()),
                    target: Some("github".to_string()),
                    title: None,
                },
            ],
            &[ExternalAgentConfigImportFailureRecord {
                item_type: "MCP_SERVER_CONFIG".to_string(),
                error_type: None,
                sub_error_type: Some("failed_to_copy_plugin_file".to_string()),
                failure_stage: "import".to_string(),
                message: "failed".to_string(),
                cwd: None,
                source: Some("broken".to_string()),
            }],
        )
        .await?;

    assert_eq!(
        runtime
            .external_agent_config_import_details_record("import-1")
            .await?,
        Some(ExternalAgentConfigImportDetailsRecord {
            successes: vec![
                ExternalAgentConfigImportSuccessRecord {
                    item_type: "CONFIG".to_string(),
                    cwd: None,
                    source: Some("settings.json".to_string()),
                    target: Some("config.toml".to_string()),
                    title: None,
                },
                ExternalAgentConfigImportSuccessRecord {
                    item_type: "MCP_SERVER_CONFIG".to_string(),
                    cwd: None,
                    source: Some("github".to_string()),
                    target: Some("github".to_string()),
                    title: None,
                }
            ],
            failures: vec![ExternalAgentConfigImportFailureRecord {
                item_type: "MCP_SERVER_CONFIG".to_string(),
                error_type: None,
                sub_error_type: Some("failed_to_copy_plugin_file".to_string()),
                failure_stage: "import".to_string(),
                message: "failed".to_string(),
                cwd: None,
                source: Some("broken".to_string()),
            }],
        })
    );
    assert_eq!(
        runtime
            .external_agent_config_import_history_records()
            .await?
            .into_iter()
            .map(|record| (
                record.import_id,
                record.provider_id,
                record.successes,
                record.failures,
                record.completed_at_ms > 0
            ))
            .collect::<Vec<_>>(),
        vec![(
            "import-1".to_string(),
            Some("provider-2".to_string()),
            vec![
                ExternalAgentConfigImportSuccessRecord {
                    item_type: "CONFIG".to_string(),
                    cwd: None,
                    source: Some("settings.json".to_string()),
                    target: Some("config.toml".to_string()),
                    title: None,
                },
                ExternalAgentConfigImportSuccessRecord {
                    item_type: "MCP_SERVER_CONFIG".to_string(),
                    cwd: None,
                    source: Some("github".to_string()),
                    target: Some("github".to_string()),
                    title: None,
                }
            ],
            vec![ExternalAgentConfigImportFailureRecord {
                item_type: "MCP_SERVER_CONFIG".to_string(),
                error_type: None,
                sub_error_type: Some("failed_to_copy_plugin_file".to_string()),
                failure_stage: "import".to_string(),
                message: "failed".to_string(),
                cwd: None,
                source: Some("broken".to_string()),
            }],
            true
        )]
    );

    Ok(())
}

#[tokio::test]
async fn reads_all_history_records() -> anyhow::Result<()> {
    let runtime = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(unique_temp_dir().as_path().abs()),
        "test-provider".to_string(),
    )
    .await?;

    runtime
        .record_external_agent_config_import_completed(
            "import-1",
            /*provider_id*/ None,
            &[],
            &[],
        )
        .await?;
    runtime
        .record_external_agent_config_import_completed(
            "import-2",
            /*provider_id*/ None,
            &[],
            &[],
        )
        .await?;

    let mut records = runtime
        .external_agent_config_import_history_records()
        .await?;
    records.sort_by(|left, right| left.import_id.cmp(&right.import_id));
    assert_eq!(
        records
            .into_iter()
            .map(|record| record.import_id)
            .collect::<Vec<_>>(),
        vec!["import-1".to_string(), "import-2".to_string()]
    );

    Ok(())
}

#[tokio::test]
async fn sqlite_memory_import_completion_enqueues_consolidation() -> anyhow::Result<()> {
    let runtime = StateRuntime::init_sqlite(unique_temp_dir(), "test-provider".to_string()).await?;
    let memory_import = ExternalAgentMemoryImport::new(
        vec!["project-a".to_string()],
        MemoryArtifactSet::new(vec![MemoryArtifact::new(
            "extensions/external_agent_import/resources/project-a/MEMORY.md",
            b"shared".to_vec(),
        )?])?,
    )?;

    runtime
        .external_agent_config_imports
        .record_completed_with_memory_import("import-memory", &[], &[], &memory_import)
        .await?;

    let claim = runtime
        .memories()
        .try_claim_global_phase2_job(codex_protocol::ThreadId::new(), /*lease_seconds*/ 60)
        .await?;
    let crate::Phase2JobClaimOutcome::Claimed {
        input_watermark, ..
    } = claim
    else {
        anyhow::bail!("expected memory import consolidation claim, got {claim:?}");
    };
    assert!(input_watermark > 0);
    Ok(())
}
