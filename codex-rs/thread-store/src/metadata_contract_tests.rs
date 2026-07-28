use std::path::Path;

use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::TurnContextItem;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::CreateThreadParams;
use crate::GitInfoPatch;
use crate::LoadThreadHistoryParams;
use crate::LocalThreadStore;
use crate::LocalThreadStoreConfig;
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
                cwd: latest_cwd.clone().try_into()?,
                workspace_roots: None,
                current_date: None,
                timezone: None,
                approval_policy: AskForApproval::Never,
                approvals_reviewer: None,
                sandbox_policy: SandboxPolicy::DangerFullAccess,
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
    store.persist_thread(cwd_thread_id).await?;
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
    store.persist_thread(thread_id).await?;
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

    let git_and_memory = update_metadata(
        store,
        thread_id,
        ThreadMetadataPatch {
            git_info: Some(GitInfoPatch {
                sha: Some(Some("contract-sha".to_string())),
                branch: Some(Some("main".to_string())),
                origin_url: Some(Some("https://example.com/contract.git".to_string())),
            }),
            memory_mode: Some(ThreadMemoryMode::Disabled),
            ..Default::default()
        },
        /*include_archived*/ false,
    )
    .await?;
    assert_eq!(
        serde_json::to_value(git_and_memory.git_info)?,
        serde_json::json!({
            "commit_hash": "contract-sha",
            "branch": "main",
            "repository_url": "https://example.com/contract.git",
        })
    );
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
        serde_json::to_value(partial_git.git_info)?,
        serde_json::json!({
            "commit_hash": "contract-sha",
            "branch": "feature/contract",
            "repository_url": "https://example.com/contract.git",
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
    store.persist_thread(paginated_id).await?;
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
