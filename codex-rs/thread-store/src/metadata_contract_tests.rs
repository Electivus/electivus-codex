use std::path::Path;

use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::TurnContextItem;
use codex_rollout::RolloutItem;
use codex_utils_absolute_path::test_support::PathExt;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::CreateThreadParams;
use crate::GitInfoPatch;
use crate::LoadThreadHistoryParams;
use crate::LocalThreadStore;
use crate::LocalThreadStoreConfig;
use crate::PersistContext;
use crate::PostgresThreadStore;
use crate::ReadThreadParams;
use crate::ThreadMetadataPatch;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;
use crate::UpdateThreadMetadataParams;
use crate::postgres_contract_tests::PostgresThreadStoreFixture;

#[tokio::test]
async fn local_metadata_contract_matches_public_thread_store_semantics()
-> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let config = LocalThreadStoreConfig {
        codex_home: home.path().to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        default_model_provider_id: "metadata-contract-provider".to_string(),
    };
    let runtime = codex_state::StateRuntime::init_sqlite(
        home.path().to_path_buf(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await?;
    let store = LocalThreadStore::new(config, Some(runtime));
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f110")?;

    assert_metadata_contract(&store, thread_id, home.path()).await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_metadata_matches_public_thread_store_semantics()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = PostgresThreadStoreFixture::new("shared_metadata")?;
    fixture.migrate().await?;
    let pool = fixture.connect_pool().await?;
    let store = PostgresThreadStore::new(pool.clone(), fixture.schema.clone());
    let thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f111")?;

    assert_metadata_contract(&store, thread_id, Path::new("/metadata-contract")).await?;
    let cwd_thread_id = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f112")?;
    store
        .create_thread(create_thread_params(
            cwd_thread_id,
            Path::new("/metadata-contract"),
            ThreadHistoryMode::Legacy,
        ))
        .await?;
    let latest_cwd = Path::new("/metadata-contract/latest-turn").to_path_buf();
    store
        .append_items(AppendThreadItemsParams {
            thread_id: cwd_thread_id,
            items: vec![RolloutItem::TurnContext(TurnContextItem {
                turn_id: Some("latest-turn".to_string()),
                cwd: PathUri::from_host_native_path(&latest_cwd)?,
                workspace_roots: None,
                current_date: None,
                timezone: None,
                approval_policy: AskForApproval::Never,
                approvals_reviewer: None,
                sandbox_policy: SandboxPolicy::DangerFullAccess.into(),
                permission_profile: None,
                network: None,
                file_system_sandbox_policy: None,
                model: "metadata-contract-model".to_string(),
                comp_hash: None,
                personality: None,
                collaboration_mode: None,
                multi_agent_version: None,
                multi_agent_mode: None,
                realtime_active: None,
                effort: None,
                summary: ReasoningSummary::Auto,
            })],
        })
        .await?;
    store
        .persist_thread(cwd_thread_id, PersistContext::Standard)
        .await?;
    store.shutdown_thread(cwd_thread_id).await?;
    let projected = read_thread(&store, cwd_thread_id, /*include_archived*/ false).await?;
    assert_eq!(projected.cwd, latest_cwd);

    pool.close().await;
    fixture.cleanup().await
}

async fn assert_metadata_contract(
    store: &dyn ThreadStore,
    thread_id: ThreadId,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    store
        .create_thread(create_thread_params(
            thread_id,
            cwd,
            ThreadHistoryMode::Legacy,
        ))
        .await?;
    store
        .persist_thread(thread_id, PersistContext::Standard)
        .await?;
    store.shutdown_thread(thread_id).await?;
    let initial = read_thread(store, thread_id, /*include_archived*/ false).await?;
    let updated_at = millisecond_timestamp(initial.updated_at + Duration::minutes(30));
    let recency_at = millisecond_timestamp(initial.recency_at + Duration::hours(1));

    let observed = update_metadata(
        store,
        thread_id,
        ThreadMetadataPatch {
            preview: Some("Contract preview".to_string()),
            first_user_message: Some("Contract preview".to_string()),
            title: Some("Distinct derived title".to_string()),
            updated_at: Some(updated_at),
            advance_recency_at: Some(recency_at),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert_eq!(observed.preview, "Contract preview");
    assert_eq!(observed.name.as_deref(), Some("Distinct derived title"));
    assert_eq!(observed.updated_at, updated_at);
    assert_eq!(observed.recency_at, recency_at);

    let matching_title = update_metadata(
        store,
        thread_id,
        ThreadMetadataPatch {
            title: Some("Contract preview".to_string()),
            advance_recency_at: Some(recency_at - Duration::minutes(1)),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert_eq!(matching_title.name, None);
    assert!(matching_title.recency_at >= recency_at);

    let explicitly_named = update_metadata(
        store,
        thread_id,
        ThreadMetadataPatch {
            name: Some(Some("Explicit name".to_string())),
            title: Some("Ignored derived title".to_string()),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert_eq!(explicitly_named.name.as_deref(), Some("Explicit name"));
    let cleared_name = update_metadata(
        store,
        thread_id,
        ThreadMetadataPatch {
            name: Some(None),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert_eq!(cleared_name.name, None);

    update_metadata(
        store,
        thread_id,
        ThreadMetadataPatch {
            git_info: Some(GitInfoPatch {
                sha: Some(Some("contract-sha".to_string())),
                branch: Some(Some("main".to_string())),
                origin_url: Some(Some("https://example.com/acme/contract.git".to_string())),
            }),
            memory_mode: Some(ThreadMemoryMode::Disabled),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert_eq!(
        latest_memory_mode(store, thread_id, /*include_archived*/ false).await?,
        "disabled"
    );

    let partial_git = update_metadata(
        store,
        thread_id,
        ThreadMetadataPatch {
            git_info: Some(GitInfoPatch {
                branch: Some(Some("feature/contract".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert_eq!(
        normalized_complete_snapshot(&partial_git),
        serde_json::json!({
            "thread_id": thread_id,
            "extra_config": null,
            "rollout_path": null,
            "forked_from_id": null,
            "parent_thread_id": null,
            "preview": "Contract preview",
            "name": null,
            "model_provider": "metadata-contract-provider",
            "model": null,
            "reasoning_effort": null,
            "created_at": "<timestamp>",
            "updated_at": "<timestamp>",
            "recency_at": "<timestamp>",
            "archived_at": null,
            "section": null,
            "section_position": null,
            "section_entered_at": null,
            "cwd": cwd,
            "cli_version": env!("CARGO_PKG_VERSION"),
            "source": SessionSource::Exec,
            "history_mode": ThreadHistoryMode::Legacy,
            "thread_source": null,
            "agent_nickname": null,
            "agent_role": null,
            "agent_path": null,
            "git_info": {
                "commit_hash": "contract-sha",
                "branch": "feature/contract",
                "repository_url": "https://example.com/acme/contract.git",
            },
            "repository_identity": "example.com/acme/contract",
            "approval_mode": AskForApproval::OnRequest,
            "permission_profile": PermissionProfile::read_only(),
            "token_usage": null,
            "first_user_message": "<backend-owned>",
            "history": null,
        })
    );
    let boundary_repo = "r".repeat(1009);
    let boundary_origin = format!("https://example.test/o/{boundary_repo}");
    let boundary_identity = format!("example.test/o/{boundary_repo}");
    assert_eq!(boundary_identity.len(), 1024);
    let boundary_git = update_metadata(
        store,
        thread_id,
        ThreadMetadataPatch {
            git_info: Some(GitInfoPatch {
                origin_url: Some(Some(boundary_origin.clone())),
                ..Default::default()
            }),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert_eq!(
        (
            boundary_git
                .git_info
                .as_ref()
                .and_then(|info| info.repository_url.as_deref()),
            boundary_git.repository_identity.as_deref(),
        ),
        (
            Some(boundary_origin.as_str()),
            Some(boundary_identity.as_str()),
        )
    );

    let oversized_origin = format!("{boundary_origin}r");
    let oversized_git = update_metadata(
        store,
        thread_id,
        ThreadMetadataPatch {
            git_info: Some(GitInfoPatch {
                origin_url: Some(Some(oversized_origin.clone())),
                ..Default::default()
            }),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert_eq!(
        (
            oversized_git
                .git_info
                .as_ref()
                .and_then(|info| info.repository_url.as_deref()),
            oversized_git.repository_identity.as_deref(),
        ),
        (Some(oversized_origin.as_str()), None)
    );

    let invalid_origin = update_metadata(
        store,
        thread_id,
        ThreadMetadataPatch {
            git_info: Some(GitInfoPatch {
                origin_url: Some(Some("https://example.com/not-a-repository.git".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert_eq!(
        normalized_complete_snapshot(&invalid_origin),
        serde_json::json!({
            "thread_id": thread_id,
            "extra_config": null,
            "rollout_path": null,
            "forked_from_id": null,
            "parent_thread_id": null,
            "preview": "Contract preview",
            "name": null,
            "model_provider": "metadata-contract-provider",
            "model": null,
            "reasoning_effort": null,
            "created_at": "<timestamp>",
            "updated_at": "<timestamp>",
            "recency_at": "<timestamp>",
            "archived_at": null,
            "section": null,
            "section_position": null,
            "section_entered_at": null,
            "cwd": cwd,
            "cli_version": env!("CARGO_PKG_VERSION"),
            "source": SessionSource::Exec,
            "history_mode": ThreadHistoryMode::Legacy,
            "thread_source": null,
            "agent_nickname": null,
            "agent_role": null,
            "agent_path": null,
            "git_info": {
                "commit_hash": "contract-sha",
                "branch": "feature/contract",
                "repository_url": "https://example.com/not-a-repository.git",
            },
            "repository_identity": null,
            "approval_mode": AskForApproval::OnRequest,
            "permission_profile": PermissionProfile::read_only(),
            "token_usage": null,
            "first_user_message": "<backend-owned>",
            "history": null,
        })
    );
    let cleared_git = update_metadata(
        store,
        thread_id,
        ThreadMetadataPatch {
            git_info: Some(GitInfoPatch {
                sha: Some(None),
                branch: Some(None),
                origin_url: Some(None),
            }),
            memory_mode: Some(ThreadMemoryMode::Enabled),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert!(cleared_git.git_info.is_none());
    assert_eq!(cleared_git.repository_identity, None);
    assert_eq!(
        latest_memory_mode(store, thread_id, /*include_archived*/ false).await?,
        "enabled"
    );

    store
        .archive_thread(ArchiveThreadParams { thread_id })
        .await?;
    let hidden_update = update_metadata(
        store,
        thread_id,
        ThreadMetadataPatch {
            preview: Some("Hidden archived preview".to_string()),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await;
    assert!(hidden_update.is_err());
    let archived = update_metadata(
        store,
        thread_id,
        ThreadMetadataPatch {
            preview: Some("Visible archived preview".to_string()),
            ..Default::default()
        },
        /*include_archived*/ true,
    )
    .await?;
    assert_eq!(archived.preview, "Visible archived preview");
    assert!(archived.archived_at.is_some());

    let paginated_id = ThreadId::new();
    store
        .create_thread(create_thread_params(
            paginated_id,
            cwd,
            ThreadHistoryMode::Paginated,
        ))
        .await?;
    store
        .persist_thread(paginated_id, PersistContext::Standard)
        .await?;
    store.shutdown_thread(paginated_id).await?;
    let derived_title = update_metadata(
        store,
        paginated_id,
        ThreadMetadataPatch {
            preview: Some("Paginated preview".to_string()),
            title: Some("Paginated derived title".to_string()),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert_eq!(derived_title.name, None);
    let named = update_metadata(
        store,
        paginated_id,
        ThreadMetadataPatch {
            name: Some(Some("Paginated explicit name".to_string())),
            title: Some("Ignored paginated title".to_string()),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert_eq!(named.name.as_deref(), Some("Paginated explicit name"));
    let cleared = update_metadata(
        store,
        paginated_id,
        ThreadMetadataPatch {
            name: Some(None),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert_eq!(cleared.name, None);
    Ok(())
}

fn normalized_complete_snapshot(thread: &crate::StoredThread) -> serde_json::Value {
    let mut snapshot = serde_json::to_value(thread).expect("StoredThread should serialize");
    snapshot["rollout_path"] = serde_json::Value::Null;
    for field in ["created_at", "updated_at", "recency_at"] {
        snapshot[field] = serde_json::Value::String("<timestamp>".to_string());
    }
    snapshot["first_user_message"] = serde_json::Value::String("<backend-owned>".to_string());
    snapshot
        .as_object_mut()
        .expect("StoredThread should serialize as an object")
        .entry("repository_identity")
        .or_insert(serde_json::Value::Null);
    snapshot
}

async fn update_metadata(
    store: &dyn ThreadStore,
    thread_id: ThreadId,
    patch: ThreadMetadataPatch,
    include_archived: bool,
) -> crate::ThreadStoreResult<crate::StoredThread> {
    store
        .update_thread_metadata(UpdateThreadMetadataParams {
            thread_id,
            patch,
            include_archived,
        })
        .await
}

async fn read_thread(
    store: &dyn ThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
) -> crate::ThreadStoreResult<crate::StoredThread> {
    store
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived,
            include_history: false,
        })
        .await
}

async fn latest_memory_mode(
    store: &dyn ThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id,
            include_archived,
        })
        .await?;
    history
        .items
        .iter()
        .rev()
        .find_map(|item| match item {
            RolloutItem::SessionMeta(meta) => meta.meta.memory_mode.clone(),
            _ => None,
        })
        .ok_or_else(|| "canonical history has no explicit memory mode".into())
}

fn millisecond_timestamp(timestamp: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(timestamp.timestamp_millis()).unwrap_or(timestamp)
}

fn create_thread_params(
    thread_id: ThreadId,
    cwd: &Path,
    history_mode: ThreadHistoryMode,
) -> CreateThreadParams {
    CreateThreadParams {
        session_id: thread_id.into(),
        thread_id,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "metadata-contract".to_string(),
        base_instructions: BaseInstructions::default(),
        dynamic_tools: Vec::new(),
        selected_capability_roots: Vec::new(),
        multi_agent_version: None,
        history_mode,
        history_base: None,
        subagent_history_start_ordinal: None,
        initial_window_id: "metadata-contract-window".to_string(),
        metadata: ThreadPersistenceMetadata {
            cwd: Some(cwd.to_path_buf()),
            model_provider: "metadata-contract-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}
