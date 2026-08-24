#![allow(
    clippy::disallowed_methods,
    reason = "PostgreSQL tests connect only to PostgreSQL pools"
)]

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use codex_app_server_protocol::ConfigReadParams;
use codex_app_server_protocol::ConfigReadResponse;
use codex_app_server_protocol::ConfigRequirementsReadResponse;
use codex_app_server_protocol::ExternalAgentConfigImportCompletedNotification;
use codex_app_server_protocol::ExternalAgentConfigImportProgressNotification;
use codex_app_server_protocol::ExternalAgentConfigImportResponse;
use codex_app_server_protocol::ExternalAgentConfigMigrationItemType;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::MemoryResetResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadDeleteParams;
use codex_app_server_protocol::ThreadDeleteResponse;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadGoalGetResponse;
use codex_app_server_protocol::ThreadGoalSetResponse;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_state::PostgresNamespaceAction;
use codex_state::PostgresNamespaceConfig;
use codex_state::PostgresPoolConfig;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use serde::de::DeserializeOwned;
use serde_json::json;
use sqlx::AssertSqlSafe;
use tempfile::TempDir;
use tokio::time::timeout;
use uuid::Uuid;

const DATABASE_URL_ENV: &str = "CODEX_TEST_POSTGRES_URL";
const RUNTIME_DATABASE_URL_ENV: &str = "CODEX_POSTGRES_URL";
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(15);
const BLOCKER_CONTENTS: &[u8] = b"configured SQLite parent must remain untouched\n";

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_app_server_initializes_from_direct_url_without_environment() -> Result<()>
{
    let database_url = std::env::var(DATABASE_URL_ENV)?;
    let schema = format!("codex_app_direct_url_{}", Uuid::new_v4().simple());
    prepare_namespace(&schema, RuntimeReadiness::Ready).await?;
    let model_server = create_mock_responses_server_repeating_assistant("unused").await;
    let home = RuntimeHome::new(&schema, &model_server.uri())?;
    let config_path = home.path().join("config.toml");
    let config = std::fs::read_to_string(&config_path)?;
    let direct_url = serde_json::to_string(&database_url)?;
    let direct_config = config.replace(
        &format!("url_env = \"{DATABASE_URL_ENV}\""),
        &format!("url = {direct_url}"),
    );
    anyhow::ensure!(
        direct_config != config,
        "URL source fixture was not replaced"
    );
    std::fs::write(&config_path, direct_config)?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .with_args(&["--strict-config"])
        .with_env_overrides(&[(DATABASE_URL_ENV, None), (RUNTIME_DATABASE_URL_ENV, None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, app_server.initialize()).await??;

    let requirements_id = app_server.send_config_requirements_read_request().await?;
    let _: ConfigRequirementsReadResponse = read_response(&mut app_server, requirements_id).await?;
    let config_id = app_server
        .send_config_read_request(ConfigReadParams {
            include_layers: false,
            cwd: None,
        })
        .await?;
    let config: ConfigReadResponse = read_response(&mut app_server, config_id).await?;
    let postgresql = config
        .config
        .additional
        .get("state")
        .and_then(|state| state.get("postgresql"))
        .context("config/read must return state.postgresql")?;
    assert_eq!(postgresql.get("url"), Some(&json!(database_url)));
    assert_eq!(postgresql.get("url_env"), None);

    assert!(app_server.shutdown_gracefully().await?.success());
    home.assert_sqlite_untouched()?;
    cleanup_schema(&database_url, &schema).await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_app_server_shares_runtime_state_without_sqlite_access() -> Result<()> {
    let database_url = std::env::var(DATABASE_URL_ENV)?;
    let schema = format!("codex_app_runtime_{}", Uuid::new_v4().simple());
    prepare_namespace(&schema, RuntimeReadiness::Ready).await?;
    let model_server = create_mock_responses_server_repeating_assistant("unused").await;
    let first_home = RuntimeHome::new(&schema, &model_server.uri())?;
    let second_home = RuntimeHome::new(&schema, &model_server.uri())?;

    let mut first = start_app_server(&first_home, &database_url).await?;
    let mut second = start_app_server(&second_home, &database_url).await?;

    let first_thread = start_thread(&mut first).await?;
    let second_thread = start_thread(&mut first).await?;
    let expected_ids = BTreeSet::from([first_thread.clone(), second_thread.clone()]);

    let first_page = list_threads(&mut second, /*cursor*/ None, /*limit*/ 1).await?;
    assert_eq!(first_page.data.len(), 1);
    let second_page = list_threads(&mut second, first_page.next_cursor, /*limit*/ 1).await?;
    assert_eq!(second_page.data.len(), 1);
    let actual_ids = first_page
        .data
        .into_iter()
        .chain(second_page.data)
        .map(|thread| thread.id)
        .collect::<BTreeSet<_>>();
    assert_eq!(actual_ids, expected_ids);

    let goal_id = first
        .send_raw_request(
            "thread/goal/set",
            Some(json!({
                "threadId": first_thread,
                "objective": "prove shared PostgreSQL Runtime State",
                "status": "paused",
            })),
        )
        .await?;
    let set_goal: ThreadGoalSetResponse = read_response(&mut first, goal_id).await?;

    let fork_id = first
        .send_thread_fork_request(ThreadForkParams {
            thread_id: first_thread.clone(),
            defer_goal_continuation: true,
            ..Default::default()
        })
        .await?;
    let forked: ThreadForkResponse = read_response(&mut first, fork_id).await?;
    assert_eq!(forked.thread.path, None);
    let forked_thread_id = forked.thread.id.clone();
    let inherited_goal_id = second
        .send_raw_request(
            "thread/goal/get",
            Some(json!({ "threadId": forked_thread_id })),
        )
        .await?;
    let inherited_goal: ThreadGoalGetResponse =
        read_response(&mut second, inherited_goal_id).await?;
    let mut expected_goal = set_goal.goal.clone();
    expected_goal.thread_id = forked_thread_id.clone();
    assert_eq!(inherited_goal.goal, Some(expected_goal));
    let expected_ids_after_fork = BTreeSet::from([
        first_thread.clone(),
        second_thread.clone(),
        forked_thread_id.clone(),
    ]);

    assert!(first.shutdown_gracefully().await?.success());
    let get_goal_id = second
        .send_raw_request(
            "thread/goal/get",
            Some(json!({ "threadId": set_goal.goal.thread_id })),
        )
        .await?;
    let get_goal: ThreadGoalGetResponse = read_response(&mut second, get_goal_id).await?;
    assert_eq!(get_goal.goal, Some(set_goal.goal));

    let resume_id = second
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: first_thread.clone(),
            exclude_turns: false,
            ..Default::default()
        })
        .await?;
    let resumed: ThreadResumeResponse = read_response(&mut second, resume_id).await?;
    assert_eq!(resumed.thread.id, first_thread);
    assert!(!resumed.thread.turns.is_empty());
    second
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: resumed.thread.id,
            input: vec![UserInput::Text {
                text: "append from the second PostgreSQL replica".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;

    let verification_pool = sqlx::PgPool::connect(&database_url).await?;
    let memory_jobs = format!("\"{schema}\".memory_jobs");
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {memory_jobs} (kind, job_key, status, retry_remaining) \
         VALUES ('memory_consolidate_global', 'global', 'pending', 3)"
    )))
    .execute(&verification_pool)
    .await?;
    let reset_id = second
        .send_raw_request("memory/reset", /*params*/ None)
        .await?;
    let _: MemoryResetResponse = read_response(&mut second, reset_id).await?;
    let remaining_memory_jobs: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {memory_jobs} WHERE kind = 'memory_consolidate_global'"
    )))
    .fetch_one(&verification_pool)
    .await?;
    assert_eq!(remaining_memory_jobs, 0);
    verification_pool.close().await;
    let after_reset = list_threads(&mut second, /*cursor*/ None, /*limit*/ 10).await?;
    assert_eq!(
        after_reset
            .data
            .into_iter()
            .map(|thread| thread.id)
            .collect::<BTreeSet<_>>(),
        expected_ids_after_fork
    );

    let delete_id = second
        .send_thread_delete_request(ThreadDeleteParams {
            thread_id: second_thread,
        })
        .await?;
    let _: ThreadDeleteResponse = read_response(&mut second, delete_id).await?;
    let after_delete = list_threads(&mut second, /*cursor*/ None, /*limit*/ 10).await?;
    assert_eq!(
        after_delete
            .data
            .into_iter()
            .map(|thread| thread.id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([first_thread, forked_thread_id])
    );

    assert!(second.shutdown_gracefully().await?.success());
    first_home.assert_sqlite_untouched()?;
    second_home.assert_sqlite_untouched()?;
    cleanup_schema(&database_url, &schema).await
}

#[tokio::test]
#[ignore = "requires the PostgreSQL Runtime State process contract environment"]
async fn postgres_contract_app_server_fails_closed_and_redacts_unavailable_url() -> Result<()> {
    const SECRET: &str = "postgres-runtime-state-secret-sentinel";
    let schema = format!("codex_app_unavailable_{}", Uuid::new_v4().simple());
    let model_server = create_mock_responses_server_repeating_assistant("unused").await;
    let home = RuntimeHome::new(&schema, &model_server.uri())?;
    let configured_url = url::Url::parse(&std::env::var(DATABASE_URL_ENV)?)?;
    let tls_query = configured_url
        .query()
        .context("PostgreSQL process contract URL must contain mTLS parameters")?;
    let unavailable_url = format!("postgresql://{SECRET}@127.0.0.1:1/codex?{tls_query}");
    let mut app_server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .with_env_overrides(&[(DATABASE_URL_ENV, Some(unavailable_url.as_str()))])
        .build()
        .await?;

    let status = timeout(Duration::from_secs(20), app_server.wait_for_exit()).await??;
    assert!(!status.success());
    let stderr = app_server
        .wait_for_stderr_containing("failed to initialize PostgreSQL Runtime State Backend")
        .await?;
    assert!(stderr.contains(DATABASE_URL_ENV));
    assert!(stderr.contains("network reachability"));
    assert!(!stderr.contains(SECRET));
    assert!(!stderr.contains(&unavailable_url));
    assert!(!stderr.contains(&home.sqlite_home().display().to_string()));
    home.assert_sqlite_untouched()
}

#[tokio::test]
#[ignore = "requires the PostgreSQL Runtime State process contract environment"]
async fn postgres_contract_app_server_rejects_missing_feature_gate_without_sqlite_fallback()
-> Result<()> {
    let database_url = std::env::var(DATABASE_URL_ENV)?;
    let schema = format!("codex_app_missing_gate_{}", Uuid::new_v4().simple());
    let model_server = create_mock_responses_server_repeating_assistant("unused").await;
    let home = RuntimeHome::new(&schema, &model_server.uri())?;
    let config_path = home.path().join("config.toml");
    let config = std::fs::read_to_string(&config_path)?;
    let config_without_gate = config.replace("postgresql_state = true\n", "");
    anyhow::ensure!(
        config_without_gate != config,
        "feature gate fixture was not removed"
    );
    std::fs::write(&config_path, config_without_gate)?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .with_env_overrides(&[(DATABASE_URL_ENV, Some(database_url.as_str()))])
        .build()
        .await?;

    let status = timeout(Duration::from_secs(20), app_server.wait_for_exit()).await??;
    assert!(!status.success());
    let stderr = app_server
        .wait_for_stderr_containing("requires `features.postgresql_state = true`")
        .await?;
    assert!(stderr.contains("state.backend"));
    home.assert_sqlite_untouched()?;
    let sqlite = codex_state::SqliteConfig::new_for_testing(home.path().abs());
    for database in sqlite.runtime_db_paths() {
        assert!(
            !database.path.exists(),
            "invalid PostgreSQL selection must not fall back to {} at {}",
            database.label,
            database.path.display()
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_external_agent_import_reports_persistence_failure() -> Result<()> {
    let database_url = std::env::var(DATABASE_URL_ENV)?;
    let schema = format!("codex_app_import_failure_{}", Uuid::new_v4().simple());
    prepare_namespace(&schema, RuntimeReadiness::Ready).await?;
    let model_server = create_mock_responses_server_repeating_assistant("unused").await;
    let home = RuntimeHome::new(&schema, &model_server.uri())?;
    let external_agent_home = home.path().join(concat!(".", "cla", "ude"));
    std::fs::create_dir_all(&external_agent_home)?;
    std::fs::write(
        external_agent_home.join("settings.json"),
        r#"{"env":{"IMPORTED":"true"}}"#,
    )?;
    let home_dir = home.path().display().to_string();
    let mut app_server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .with_env_overrides(&[
            (DATABASE_URL_ENV, Some(database_url.as_str())),
            ("HOME", Some(home_dir.as_str())),
        ])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, app_server.initialize()).await??;

    let pool = sqlx::PgPool::connect(&database_url).await?;
    sqlx::query(AssertSqlSafe(format!(
        "DROP TABLE \"{schema}\".external_agent_config_imports"
    )))
    .execute(&pool)
    .await?;
    pool.close().await;

    let request_id = app_server
        .send_raw_request(
            "externalAgentConfig/import",
            Some(json!({
                "migrationItems": [{
                    "itemType": "CONFIG",
                    "description": "Import config",
                    "cwd": null
                }]
            })),
        )
        .await?;
    let response: ExternalAgentConfigImportResponse =
        read_response(&mut app_server, request_id).await?;
    let progress = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("externalAgentConfig/import/progress"),
    )
    .await??;
    let progress: ExternalAgentConfigImportProgressNotification =
        serde_json::from_value(progress.params.expect("progress params"))?;
    assert_eq!(progress.import_id, response.import_id);
    assert_eq!(progress.item_type_results.len(), 1);
    assert_eq!(progress.item_type_results[0].successes.len(), 1);
    assert_eq!(progress.item_type_results[0].failures, Vec::new());

    let completed = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_notification_message("externalAgentConfig/import/completed"),
    )
    .await??;
    let completed: ExternalAgentConfigImportCompletedNotification =
        serde_json::from_value(completed.params.expect("completed params"))?;
    assert_eq!(completed.import_id, response.import_id);
    assert_eq!(completed.item_type_results.len(), 1);
    let result = &completed.item_type_results[0];
    assert_eq!(
        result.item_type,
        ExternalAgentConfigMigrationItemType::Config
    );
    assert_eq!(result.successes, Vec::new());
    assert_eq!(result.failures.len(), 1);
    assert_eq!(
        result.failures[0].error_type.as_deref(),
        Some("runtime_state_unavailable")
    );
    assert_eq!(
        result.failures[0].failure_stage,
        "persist_import_completion"
    );
    assert_eq!(
        result.failures[0].message,
        "failed to persist import completion; verify Runtime State health and retry"
    );
    assert!(!result.failures[0].message.contains("postgres"));
    assert!(!result.failures[0].message.contains(&schema));
    assert!(!result.failures[0].message.contains(&database_url));

    assert!(app_server.shutdown_gracefully().await?.success());
    home.assert_sqlite_untouched()?;
    cleanup_schema(&database_url, &schema).await
}

async fn start_app_server(home: &RuntimeHome, database_url: &str) -> Result<TestAppServer> {
    let mut app_server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .with_env_overrides(&[(DATABASE_URL_ENV, Some(database_url))])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, app_server.initialize()).await??;
    Ok(app_server)
}

async fn start_thread(app_server: &mut TestAppServer) -> Result<String> {
    let request_id = app_server
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?;
    let response: ThreadStartResponse = read_response(app_server, request_id).await?;
    app_server
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: response.thread.id.clone(),
            input: vec![UserInput::Text {
                text: "persist this thread through PostgreSQL".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    Ok(response.thread.id)
}

async fn list_threads(
    app_server: &mut TestAppServer,
    cursor: Option<String>,
    limit: u32,
) -> Result<ThreadListResponse> {
    let request_id = app_server
        .send_thread_list_request(ThreadListParams {
            cursor,
            limit: Some(limit),
            sort_key: None,
            sort_direction: Some(SortDirection::Asc),
            model_providers: Some(Vec::new()),
            source_kinds: Some(Vec::new()),
            archived: Some(false),
            section_id: None,
            project_id: None,
            cwd: None,
            use_state_db_only: true,
            search_term: None,
            parent_thread_id: None,
            ancestor_thread_id: None,
            project_cwd: None,
        })
        .await?;
    read_response(app_server, request_id).await
}

async fn read_response<T: DeserializeOwned>(
    app_server: &mut TestAppServer,
    request_id: i64,
) -> Result<T> {
    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    to_response(response)
}

enum RuntimeReadiness {
    Ready,
}

async fn prepare_namespace(schema: &str, readiness: RuntimeReadiness) -> Result<()> {
    let config = PostgresNamespaceConfig::new(
        DATABASE_URL_ENV.to_string(),
        schema.to_string(),
        PostgresPoolConfig::default(),
    )?;
    codex_state::manage_postgres_namespace(config, PostgresNamespaceAction::Migrate).await?;
    match readiness {
        RuntimeReadiness::Ready => mark_namespace_ready(schema).await,
    }
}

async fn mark_namespace_ready(schema: &str) -> Result<()> {
    let database_url = std::env::var(DATABASE_URL_ENV)?;
    let pool = sqlx::PgPool::connect(&database_url).await?;
    let migration = format!("\"{schema}\".runtime_state_migration");
    let evidence = json!({
        "sourceIdentity": "app-server-process-contract",
        "sourceFingerprint": "app-server-process-contract-fingerprint",
        "phase": "ready",
        "ready": true,
        "fencingToken": 4,
        "namespaceDigest": "app-server-process-contract-digest",
        "globalReferentialIntegrityValidated": true,
        "canonicalThreadHistoryOrderingValidated": true,
    });
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {migration} (source_identity, source_fingerprint, phase, ready, \
         phase_evidence, fencing_token) VALUES ($1, $2, 'ready', TRUE, $3, 4)"
    )))
    .bind("app-server-process-contract")
    .bind("app-server-process-contract-fingerprint")
    .bind(evidence)
    .execute(&pool)
    .await?;
    pool.close().await;
    Ok(())
}

async fn cleanup_schema(database_url: &str, schema: &str) -> Result<()> {
    let pool = sqlx::PgPool::connect(database_url).await?;
    sqlx::query(AssertSqlSafe(format!("DROP SCHEMA \"{schema}\" CASCADE")))
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}

struct RuntimeHome {
    directory: TempDir,
    blocker: std::path::PathBuf,
    sqlite_home: std::path::PathBuf,
}

impl RuntimeHome {
    fn new(schema: &str, model_server_url: &str) -> Result<Self> {
        let directory = TempDir::new()?;
        let blocker = directory.path().join("sqlite-parent-is-a-file");
        std::fs::write(&blocker, BLOCKER_CONTENTS)?;
        let sqlite_home = blocker.join("must-never-be-statted-or-created");
        let sqlite_home_toml = serde_json::to_string(&sqlite_home.display().to_string())?;
        let model_server_url = serde_json::to_string(&format!("{model_server_url}/v1"))?;
        std::fs::write(
            directory.path().join("config.toml"),
            format!(
                r#"sqlite_home = {sqlite_home_toml}
model = "gpt-5.4"
model_provider = "mock_provider"

[features]
goals = true
postgresql_state = true

[state]
backend = "postgresql"

[state.postgresql]
url_env = "{DATABASE_URL_ENV}"
schema = "{schema}"

[model_providers.mock_provider]
name = "PostgreSQL process contract provider"
base_url = {model_server_url}
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
            ),
        )?;
        Ok(Self {
            directory,
            blocker,
            sqlite_home,
        })
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn sqlite_home(&self) -> &Path {
        &self.sqlite_home
    }

    fn assert_sqlite_untouched(&self) -> Result<()> {
        assert_eq!(std::fs::read(&self.blocker)?, BLOCKER_CONTENTS);
        assert!(
            !self.sqlite_home.exists(),
            "PostgreSQL startup must not create the configured SQLite home"
        );
        let sqlite = codex_state::SqliteConfig::new_for_testing(self.sqlite_home.abs());
        for database in sqlite.runtime_db_paths() {
            assert!(
                !database.path.exists(),
                "PostgreSQL startup must not create {} at {}",
                database.label,
                database.path.display()
            );
        }
        let blocker_metadata = std::fs::metadata(&self.blocker)
            .with_context(|| format!("stat unchanged blocker {}", self.blocker.display()))?;
        assert!(blocker_metadata.is_file());
        Ok(())
    }
}
