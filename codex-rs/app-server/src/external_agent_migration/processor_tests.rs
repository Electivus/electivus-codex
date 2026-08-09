use super::*;
use codex_app_server_protocol::ExternalAgentConfigImportItemTypeSuccess as ProtocolImportSuccess;
use codex_app_server_protocol::ExternalAgentConfigImportTypeResult as ProtocolImportTypeResult;
use pretty_assertions::assert_eq;

fn migration_item(
    item_type: ExternalAgentConfigMigrationItemType,
) -> ExternalAgentConfigMigrationItem {
    ExternalAgentConfigMigrationItem {
        item_type,
        description: String::new(),
        cwd: None,
        details: None,
    }
}

#[test]
fn migration_items_that_update_runtime_sources_trigger_refresh() {
    assert!(migration_items_need_runtime_refresh(&[migration_item(
        ExternalAgentConfigMigrationItemType::Config,
    )]));
    assert!(migration_items_need_runtime_refresh(&[migration_item(
        ExternalAgentConfigMigrationItemType::Skills,
    )]));
    assert!(migration_items_need_runtime_refresh(&[migration_item(
        ExternalAgentConfigMigrationItemType::McpServerConfig,
    )]));
    assert!(migration_items_need_runtime_refresh(&[migration_item(
        ExternalAgentConfigMigrationItemType::Hooks,
    )]));
    assert!(migration_items_need_runtime_refresh(&[migration_item(
        ExternalAgentConfigMigrationItemType::Commands,
    )]));
    assert!(migration_items_need_runtime_refresh(&[migration_item(
        ExternalAgentConfigMigrationItemType::Plugins,
    )]));
    assert!(!migration_items_need_runtime_refresh(&[migration_item(
        ExternalAgentConfigMigrationItemType::Memory,
    )]));
    assert!(!migration_items_need_runtime_refresh(&[migration_item(
        ExternalAgentConfigMigrationItemType::Sessions,
    )]));
}

#[test]
fn import_persistence_failure_replaces_successes_with_backend_neutral_failures() {
    let mut notification = ExternalAgentConfigImportCompletedNotification {
        import_id: "import-1".to_string(),
        item_type_results: vec![ProtocolImportTypeResult {
            item_type: ExternalAgentConfigMigrationItemType::Memory,
            successes: vec![ProtocolImportSuccess {
                item_type: ExternalAgentConfigMigrationItemType::Memory,
                cwd: None,
                source: Some("claude".to_string()),
                target: Some("MEMORY.md".to_string()),
                title: None,
            }],
            failures: Vec::new(),
        }],
    };

    mark_import_persistence_failed(&mut notification);

    assert_eq!(
        notification,
        ExternalAgentConfigImportCompletedNotification {
            import_id: "import-1".to_string(),
            item_type_results: vec![ProtocolImportTypeResult {
                item_type: ExternalAgentConfigMigrationItemType::Memory,
                successes: Vec::new(),
                failures: vec![ProtocolImportFailure {
                    item_type: ExternalAgentConfigMigrationItemType::Memory,
                    error_type: Some("runtime_state_unavailable".to_string()),
                    sub_error_type: None,
                    failure_stage: "persist_import_completion".to_string(),
                    message:
                        "failed to persist import completion; verify Runtime State health and retry"
                            .to_string(),
                    cwd: None,
                    source: None,
                }],
            }],
        }
    );
}
