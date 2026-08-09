use super::CanonicalThreadHistoryReader;
use super::RuntimeStateMigrationPhase;
use super::RuntimeStateThreadProjectionMaterializer;
use super::import_runtime_state_operational;
use super::import_runtime_state_threads;
use super::preflight_runtime_state_migration;
use super::test_support;
use crate::ExternalAgentConfigImportFailureRecord;
use crate::ExternalAgentConfigImportSuccessRecord;
use crate::GoalAccountingMode;
use crate::GoalAccountingOutcome;
use crate::GoalAccountingRequest;
use crate::GoalAccountingTarget;
use crate::LogEntry;
use crate::LogQuery;
use crate::PostgresNamespaceAction;
use crate::PostgresNamespaceConfig;
use crate::PostgresPoolConfig;
use crate::PostgresRuntimeStatePool;
use crate::RemoteControlEnrollmentRecord;
use crate::SqliteConfig;
use crate::ThreadGoalStatus;
use crate::manage_postgres_namespace;
use crate::postgres::qualified_table;
use crate::postgres::quote_identifier;
use crate::runtime::LogStore;
use crate::runtime::test_support::test_thread_metadata;
use anyhow::Context;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Row;
use std::convert::Infallible;
use std::path::Path;
use uuid::Uuid;

struct FixtureHistoryReader {
    lines: Vec<RolloutLine>,
}

impl CanonicalThreadHistoryReader for FixtureHistoryReader {
    async fn read(
        &self,
        _path: &Path,
        maximum_source_bytes: u64,
    ) -> anyhow::Result<(Vec<RolloutLine>, usize, u64)> {
        let source_bytes = u64::try_from(serde_json::to_vec(&self.lines)?.len())?;
        anyhow::ensure!(
            source_bytes <= maximum_source_bytes,
            "fixture exceeds read budget"
        );
        Ok((self.lines.clone(), 0, source_bytes))
    }
}

struct EmptyProjectionMaterializer {
    schema: String,
}

impl RuntimeStateThreadProjectionMaterializer for EmptyProjectionMaterializer {
    type Error = Infallible;

    fn destination_schema(&self) -> &str {
        &self.schema
    }

    async fn materialize(
        &self,
        _connection: &mut sqlx::PgConnection,
        _snapshot: &super::RuntimeStateThreadSnapshot,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_imports_complete_operational_state_read_only() -> anyhow::Result<()> {
    let (source, runtime) = test_support::initialized_runtime_source("operational").await?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let thread_id = ThreadId::from_string("019c84d0-3333-7777-8333-333333333333")?;
    let rollout_path = source.join("sessions/2026/07/22/operational.jsonl");
    std::fs::create_dir_all(rollout_path.parent().expect("rollout parent"))?;
    let mut metadata = test_thread_metadata(&source, thread_id, source.join("workspace"));
    metadata.rollout_path = rollout_path.clone();
    runtime.upsert_thread(&metadata).await?;
    let history = vec![RolloutLine {
        timestamp: "2026-07-22T10:00:00Z".to_string(),
        ordinal: None,
        item: RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                timestamp: "2026-07-22T10:00:00Z".to_string(),
                cwd: source.join("workspace"),
                source: SessionSource::Cli,
                model_provider: Some("test-provider".to_string()),
                ..SessionMeta::default()
            },
            git: None,
        }),
    }];
    std::fs::write(
        &rollout_path,
        format!("{}\n", serde_json::to_string(&history[0])?),
    )?;

    let now = chrono::Utc::now().timestamp();
    let logs = [
        log_entry(
            now,
            /*ts_nanos*/ 10,
            "first body",
            Some(thread_id.to_string()),
        ),
        log_entry(
            now,
            /*ts_nanos*/ 20,
            "process body",
            /*thread_id*/ None,
        ),
    ];
    runtime.insert_logs(&logs).await?;
    let expected_logs = runtime.query_logs(&LogQuery::default()).await?;
    let goal = runtime
        .thread_goals()
        .replace_thread_goal(
            thread_id,
            "finish the operational migration",
            ThreadGoalStatus::Active,
            /*token_budget*/ Some(1_000),
        )
        .await?;
    let accounting = GoalAccountingRequest {
        event_id: "migration-accounting",
        time_delta_seconds: 7,
        token_delta: 40,
        mode: GoalAccountingMode::ActiveOnly,
        target: GoalAccountingTarget::GoalId(&goal.goal_id),
    };
    let GoalAccountingOutcome::Updated(expected_goal) = runtime
        .thread_goals()
        .account_thread_goal_usage(thread_id, accounting)
        .await?
    else {
        anyhow::bail!("fixture accounting was not applied");
    };
    runtime
        .thread_goals()
        .replace_thread_goal_snapshot(&expected_goal)
        .await?;
    let enrollment = RemoteControlEnrollmentRecord {
        websocket_url: "wss://example.test/remote".to_string(),
        account_id: "account-migration".to_string(),
        app_server_client_name: Some("desktop".to_string()),
        server_id: "server-migration".to_string(),
        environment_id: "environment-migration".to_string(),
        server_name: "Migration server".to_string(),
        remote_control_enabled: Some(false),
    };
    runtime
        .upsert_remote_control_enrollment(&enrollment)
        .await?;
    runtime
        .record_external_agent_config_import_completed(
            "import-migration",
            Some("migration-provider"),
            &[ExternalAgentConfigImportSuccessRecord {
                item_type: "skills".to_string(),
                cwd: Some(source.join("workspace")),
                source: Some("source-skill".to_string()),
                target: Some("target-skill".to_string()),
                title: None,
            }],
            &[ExternalAgentConfigImportFailureRecord {
                item_type: "hooks".to_string(),
                error_type: Some("invalid_hook".to_string()),
                sub_error_type: Some("unsupported_event".to_string()),
                failure_stage: "write".to_string(),
                message: "unsupported hook".to_string(),
                cwd: None,
                source: Some("source-hook".to_string()),
            }],
        )
        .await?;
    let expected_imports = runtime
        .external_agent_config_import_history_records()
        .await?;
    runtime.close().await;

    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?);
    let logs_pool = sqlite.open_read_write_pool(&sqlite.logs_db_path()).await?;
    sqlx::query("UPDATE logs SET estimated_bytes = ? WHERE id = 1")
        .bind(10_i64 * 1024 * 1024)
        .execute(&logs_pool)
        .await?;
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&logs_pool)
        .await?;
    logs_pool.close().await;
    let source_before = test_support::snapshot_source(&source)?;

    let destination = PostgresNamespaceConfig::new(
        "CODEX_TEST_POSTGRES_URL".to_string(),
        format!("CodexMigration{}", Uuid::new_v4().simple()),
        PostgresPoolConfig::default(),
    )?;
    manage_postgres_namespace(destination.clone(), PostgresNamespaceAction::Migrate).await?;
    let inventory = preflight_runtime_state_migration(sqlite.clone(), destination.clone()).await?;
    let materializer = EmptyProjectionMaterializer {
        schema: destination.schema().to_string(),
    };
    import_runtime_state_threads(
        &sqlite,
        &destination,
        &inventory,
        &FixtureHistoryReader { lines: history },
        &materializer,
    )
    .await?;
    let progress = import_runtime_state_operational(&sqlite, &destination, &inventory).await?;
    assert_eq!(
        progress.phase(),
        RuntimeStateMigrationPhase::OperationalImported
    );
    assert_eq!(
        import_runtime_state_operational(&sqlite, &destination, &inventory,).await?,
        progress
    );

    let pool = PostgresRuntimeStatePool::connect_for_migration(destination.clone()).await?;
    let (raw_pool, schema) = pool.thread_store_connection();
    let log_store = LogStore::from_postgres(raw_pool.clone(), schema.clone());
    assert_eq!(
        log_store.query_logs(&LogQuery::default()).await?,
        expected_logs
    );
    assert_eq!(
        log_store
            .query_logs(&LogQuery {
                module_like: vec!["migration::operational".to_string()],
                ..LogQuery::default()
            })
            .await?,
        expected_logs
    );
    let goal_store = pool.goal_store();
    assert_eq!(
        goal_store.get_thread_goal(thread_id).await?,
        Some(expected_goal.clone())
    );
    assert!(
        goal_store
            .has_thread_goal_continuation_deferral(thread_id)
            .await?
    );
    assert_eq!(
        goal_store
            .account_thread_goal_usage(thread_id, accounting)
            .await?,
        GoalAccountingOutcome::AlreadyAccounted(expected_goal)
    );
    let enrollment_store = pool.remote_control_enrollment_store();
    assert_eq!(
        enrollment_store
            .get(
                &enrollment.websocket_url,
                &enrollment.account_id,
                enrollment.app_server_client_name.as_deref(),
            )
            .await?,
        Some(enrollment.clone())
    );
    assert_eq!(
        pool.external_agent_config_import_store().history().await?,
        expected_imports
    );
    let migration = qualified_table(&schema, "runtime_state_migration");
    let evidence = sqlx::query(AssertSqlSafe(format!(
        "SELECT phase, ready, phase_evidence, fencing_token FROM {migration} WHERE singleton"
    )))
    .fetch_one(&raw_pool)
    .await?;
    assert_eq!(
        (
            evidence.try_get::<String, _>("phase")?,
            evidence.try_get::<bool, _>("ready")?,
            evidence.try_get::<i64, _>("fencing_token")?,
        ),
        ("operational_imported".to_string(), false, 2)
    );
    let phase_evidence: Value = evidence.try_get("phase_evidence")?;
    let namespace_digest = phase_evidence["namespaceDigest"]
        .as_str()
        .context("migration evidence has no namespace digest")?
        .to_string();
    let threads_content_hash = phase_evidence["threadsContentHash"]
        .as_str()
        .context("migration evidence has no thread content hash")?
        .to_string();
    let history_content_hash = phase_evidence["historyContentHash"]
        .as_str()
        .context("migration evidence has no history content hash")?
        .to_string();
    let coordination_content_hash = phase_evidence["threadCoordinationContentHash"]
        .as_str()
        .context("migration evidence has no thread coordination content hash")?
        .to_string();
    assert_eq!(
        phase_evidence,
        serde_json::json!({
            "sourceIdentity": super::import_threads::source_identity(&sqlite),
            "sourceFingerprint": super::import_threads::fingerprint(&inventory),
            "phase": "operational_imported",
            "ready": false,
            "fencingToken": 2,
            "threads": 1,
            "historyLines": 1,
            "threadsContentHash": threads_content_hash,
            "historyContentHash": history_content_hash,
            "threadCoordinationContentHash": coordination_content_hash,
            "logs": 2,
            "goals": 1,
            "goalDeferrals": 1,
            "goalAccountingEvents": 1,
            "remoteControlEnrollments": 1,
            "externalAgentConfigImports": 1,
            "namespaceDigest": namespace_digest,
        })
    );

    log_store
        .insert_log(&log_entry(
            now,
            /*ts_nanos*/ 30,
            "after migration",
            Some(thread_id.to_string()),
        ))
        .await?;
    assert_eq!(
        log_store
            .query_logs(&LogQuery::default())
            .await?
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(
        enrollment_store
            .set_enabled(
                &enrollment.websocket_url,
                &enrollment.account_id,
                enrollment.app_server_client_name.as_deref(),
                /*remote_control_enabled*/ true,
            )
            .await?,
        1
    );
    assert_eq!(
        enrollment_store
            .get(
                &enrollment.websocket_url,
                &enrollment.account_id,
                enrollment.app_server_client_name.as_deref(),
            )
            .await?
            .and_then(|record| record.remote_control_enabled),
        Some(true)
    );
    assert_eq!(test_support::snapshot_source(&source)?, source_before);

    sqlx::query(AssertSqlSafe(format!(
        "DROP SCHEMA {} CASCADE",
        quote_identifier(destination.schema())
    )))
    .execute(&raw_pool)
    .await?;
    pool.close().await;
    Ok(())
}

fn log_entry(ts: i64, ts_nanos: i64, body: &str, thread_id: Option<String>) -> LogEntry {
    LogEntry {
        ts,
        ts_nanos,
        level: "INFO".to_string(),
        target: "migration".to_string(),
        message: None,
        feedback_log_body: Some(body.to_string()),
        thread_id,
        process_uuid: Some("process-migration".to_string()),
        module_path: Some("migration::operational".to_string()),
        file: Some("migration.rs".to_string()),
        line: Some(28),
    }
}
