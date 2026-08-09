use super::ExternalAgentConfigImportFailureRecord;
use super::ExternalAgentConfigImportStore;
use super::ExternalAgentConfigImportSuccessRecord;
use anyhow::Result;
use chrono::Utc;
use pretty_assertions::assert_eq;
use std::path::PathBuf;

pub(crate) async fn run_external_agent_config_import_contract(
    writer: &ExternalAgentConfigImportStore,
    reader: &ExternalAgentConfigImportStore,
) -> Result<()> {
    let successes = vec![
        ExternalAgentConfigImportSuccessRecord {
            item_type: "MEMORY".to_string(),
            cwd: Some(PathBuf::from("/workspace/alpha")),
            source: Some("project-alpha/MEMORY.md".to_string()),
            target: Some(
                "extensions/external_agent_import/resources/project-alpha/MEMORY.md".to_string(),
            ),
            title: None,
        },
        ExternalAgentConfigImportSuccessRecord {
            item_type: "SKILLS".to_string(),
            cwd: None,
            source: Some("review".to_string()),
            target: Some("review".to_string()),
            title: None,
        },
    ];
    let failures = vec![ExternalAgentConfigImportFailureRecord {
        item_type: "MEMORY".to_string(),
        error_type: Some("memory_import".to_string()),
        sub_error_type: Some("invalid_scope".to_string()),
        failure_stage: "import".to_string(),
        message: "project scope is unavailable".to_string(),
        cwd: Some(PathBuf::from("/workspace/beta")),
        source: Some("project-beta".to_string()),
    }];

    writer
        .record_completed("import-alpha", &successes, &failures)
        .await?;
    writer
        .record_completed("import-alpha", &successes, &failures)
        .await?;

    assert_eq!(
        reader.details("import-alpha").await?,
        Some(super::ExternalAgentConfigImportDetailsRecord {
            successes: successes.clone(),
            failures: failures.clone(),
        })
    );
    let alpha_completed_at = reader.history().await?[0].completed_at_ms;
    while Utc::now().timestamp_millis() <= alpha_completed_at {
        tokio::task::yield_now().await;
    }
    writer.record_completed("import-beta", &[], &[]).await?;

    let history = reader.history().await?;
    assert_eq!(
        history
            .iter()
            .map(|record| record.import_id.as_str())
            .collect::<Vec<_>>(),
        vec!["import-beta", "import-alpha"]
    );
    assert_eq!(history[0].successes, vec![]);
    assert_eq!(history[0].failures, vec![]);
    assert_eq!(history[1].successes, successes);
    assert_eq!(history[1].failures, failures);
    assert!(history[1].completed_at_ms > 0);

    Ok(())
}
