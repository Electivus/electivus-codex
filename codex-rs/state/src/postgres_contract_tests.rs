use super::MAXIMUM_COMPATIBLE_SCHEMA_VERSION;
use super::MigrationHistory;
use super::PostgresNamespaceAction;
use super::PostgresRuntimeStatePool;
use super::test_support::PostgresContractFixture;
use super::test_support::test_database_url;
use crate::migrations::tests::repository_identity_cases;
use crate::runtime::LogStore;
use crate::runtime::RemoteControlEnrollmentStore;
use crate::runtime::backfill_contract_tests::run_backfill_coordination_contract;
use crate::runtime::external_agent_config_imports_contract_tests::run_external_agent_config_import_contract;
use crate::runtime::goals_contract_tests::collect_closed_goal_store_errors;
use crate::runtime::goals_contract_tests::goal_store_error_signature;
use crate::runtime::goals_contract_tests::provoke_goal_accounting_conflict;
use crate::runtime::goals_contract_tests::run_goal_lifecycle_contract;
use crate::runtime::logs_contract_tests::run_feedback_contract;
use crate::runtime::logs_contract_tests::run_filter_order_and_max_id_contract;
use crate::runtime::logs_contract_tests::run_partition_limits_contract;
use crate::runtime::logs_contract_tests::run_replica_visibility_contract;
use crate::runtime::logs_contract_tests::run_startup_retention_contract;
use crate::runtime::memory_store_contract_tests::run_stage1_claim_and_output_contract;
use crate::runtime::memory_store_contract_tests::run_stage1_retry_and_lease_contract;
use crate::runtime::memory_store_output_contract_tests::run_postgres_stage1_output_data_contract;
use crate::runtime::memory_store_phase2_contract_tests::run_phase2_enqueue_and_claim_contract;
use crate::runtime::memory_store_phase2_contract_tests::run_phase2_heartbeat_and_failure_contract;
use crate::runtime::memory_store_phase2_success_contract_tests::phase2_success_thread_ids;
use crate::runtime::memory_store_phase2_success_contract_tests::run_phase2_success_contract;
use crate::runtime::memory_store_phase2_success_contract_tests::seed_postgres_phase2_success_threads;
use crate::runtime::memory_store_startup_contract_tests::run_postgres_stage1_startup_contract;
use crate::runtime::remote_control_contract_tests::run_remote_control_enrollment_contract;
use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;
use std::collections::HashMap;
use std::time::Duration;

const REPOSITORY_IDENTITY_MIGRATION_VERSION: i64 = 21;

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_reads_resume_metadata_and_deletes_integrally() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "runtime_lifecycle")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    fixture.mark_runtime_ready_for_tests().await?;
    let pool = fixture.connect_pool().await?;
    let thread_id = ThreadId::new();
    let cwd = crate::runtime::test_support::unique_temp_dir();
    let threads = super::qualified_table(fixture.schema(), "threads");
    let logs = super::qualified_table(fixture.schema(), "logs");
    let projection = serde_json::json!({
        "cwd": cwd.clone(),
        "model": "gpt-runtime-state-contract",
    });
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {threads} (thread_id, projection, stream_version, fencing_token, writer_id, \
         writer_lease_expires_at, created_at, updated_at, recency_at) \
         VALUES ($1, $2, 0, 1, 'runtime-contract', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, \
         CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
    )))
    .bind(thread_id.to_string())
    .bind(projection)
    .execute(&pool)
    .await?;
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {logs} (ts, ts_nanos, level, target, thread_id, estimated_bytes) \
         VALUES (1, 0, 'INFO', 'runtime-contract', $1, 1)"
    )))
    .bind(thread_id.to_string())
    .execute(&pool)
    .await?;

    let runtime = crate::StateRuntime::init_with_backend(
        crate::RuntimeStateBackendConfig::Postgresql {
            codex_home: AbsolutePathBuf::try_from(cwd.clone())?,
            namespace: fixture.config_for_tests(),
        },
        "test-provider".to_string(),
    )
    .await?;
    assert_eq!(
        runtime.get_thread_resume_metadata(thread_id).await?,
        Some(crate::ThreadResumeMetadata {
            cwd,
            model: Some("gpt-runtime-state-contract".to_string()),
        })
    );
    assert!(
        runtime
            .set_thread_preview_if_empty(thread_id, "shared goal preview")
            .await?
    );
    let preview: String = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT projection ->> 'preview' FROM {threads} WHERE thread_id = $1"
    )))
    .bind(thread_id.to_string())
    .fetch_one(&pool)
    .await?;
    assert_eq!(preview, "shared goal preview");
    assert_eq!(runtime.delete_thread(thread_id).await?, 1);
    let remaining: (i64, i64) = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT (SELECT COUNT(*) FROM {threads} WHERE thread_id = $1), \
         (SELECT COUNT(*) FROM {logs} WHERE thread_id = $1)"
    )))
    .bind(thread_id.to_string())
    .fetch_one(&pool)
    .await?;
    assert_eq!(remaining, (0, 0));

    runtime.close().await;
    pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_thread_sections_move_order_and_project_atomically() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "thread_sections")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    fixture.mark_runtime_ready_for_tests().await?;
    let pool = fixture.connect_pool().await?;
    let threads = super::qualified_table(fixture.schema(), "threads");
    let first = ThreadId::new();
    let second = ThreadId::new();
    for thread_id in [first, second] {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {threads} (thread_id, projection, stream_version, fencing_token, \
             writer_id, writer_lease_expires_at, created_at, updated_at, recency_at) \
             VALUES ($1, '{{}}', 0, 1, 'section-contract', CURRENT_TIMESTAMP, \
             CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
        )))
        .bind(thread_id.to_string())
        .execute(&pool)
        .await?;
    }

    let codex_home = crate::runtime::test_support::unique_temp_dir();
    let runtime = crate::StateRuntime::init_with_backend(
        crate::RuntimeStateBackendConfig::Postgresql {
            codex_home: AbsolutePathBuf::try_from(codex_home)?,
            namespace: fixture.config_for_tests(),
        },
        "test-provider".to_string(),
    )
    .await?;
    let pinned = crate::ThreadSection {
        id: crate::PINNED_THREAD_SECTION_ID.to_string(),
        name: crate::PINNED_THREAD_SECTION_NAME.to_string(),
    };
    assert_eq!(
        runtime.get_thread_section(&pinned.id).await?,
        Some(pinned.clone())
    );
    assert_eq!(
        runtime
            .list_thread_sections(/*cursor*/ None, /*limit*/ 1)
            .await?,
        crate::ThreadSectionsPage {
            sections: vec![pinned.clone()],
            next_cursor: None,
        }
    );

    assert!(
        runtime
            .move_thread_to_section(first, Some(&pinned.id), /*before_thread_id*/ None,)
            .await?
    );
    assert!(
        runtime
            .move_thread_to_section(
                second,
                Some(&pinned.id),
                /*before_thread_id*/ Some(first),
            )
            .await?
    );

    let ordering = runtime
        .get_thread_section_ordering(&[first, second])
        .await?;
    let first_entered_at = ordering
        .get(&first)
        .and_then(|(_, entered_at)| *entered_at)
        .expect("first section entry time should be recorded");
    let second_entered_at = ordering
        .get(&second)
        .and_then(|(_, entered_at)| *entered_at)
        .expect("second section entry time should be recorded");
    assert_eq!(
        ordering,
        HashMap::from([
            (first, (Some(1_000_000), Some(first_entered_at))),
            (second, (Some(500_000), Some(second_entered_at))),
        ])
    );
    let rows = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            Option<i64>,
            Option<DateTime<Utc>>,
            Option<String>,
            Option<i64>,
            Option<DateTime<Utc>>,
        ),
    >(AssertSqlSafe(format!(
        "SELECT thread_id, thread_section_id, section_position, section_entered_at, \
         projection -> 'section' ->> 'id', \
         (projection ->> 'section_position')::BIGINT, \
         (projection ->> 'section_entered_at')::TIMESTAMPTZ \
         FROM {threads} ORDER BY section_position, thread_id"
    )))
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        rows,
        vec![
            (
                second.to_string(),
                Some(pinned.id.clone()),
                Some(500_000),
                Some(second_entered_at),
                Some(pinned.id.clone()),
                Some(500_000),
                Some(second_entered_at),
            ),
            (
                first.to_string(),
                Some(pinned.id),
                Some(1_000_000),
                Some(first_entered_at),
                Some(crate::PINNED_THREAD_SECTION_ID.to_string()),
                Some(1_000_000),
                Some(first_entered_at),
            ),
        ]
    );

    assert!(
        runtime
            .move_thread_to_section(first, /*section*/ None, /*before_thread_id*/ None)
            .await?
    );
    let cleared = sqlx::query_as::<
        _,
        (
            Option<String>,
            Option<i64>,
            Option<DateTime<Utc>>,
            serde_json::Value,
            serde_json::Value,
            serde_json::Value,
        ),
    >(AssertSqlSafe(format!(
        "SELECT thread_section_id, section_position, section_entered_at, \
         projection -> 'section', projection -> 'section_position', \
         projection -> 'section_entered_at' FROM {threads} WHERE thread_id = $1"
    )))
    .bind(first.to_string())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        cleared,
        (
            None,
            None,
            None,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        )
    );

    runtime.close().await;
    pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_repository_identity_migration_preserves_projection_and_is_replica_visible()
-> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "repository_identity")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let pool = fixture.connect_pool().await?;
    let threads = super::qualified_table(fixture.schema(), "threads");
    let migrations = super::qualified_migration_table(fixture.schema());
    for index in [
        "threads_section_position_idx",
        "threads_section_recency_idx",
        "threads_repository_identity_created_idx",
        "threads_repository_identity_updated_idx",
        "threads_repository_identity_recency_idx",
    ] {
        let index = super::qualified_table(fixture.schema(), index);
        sqlx::query(AssertSqlSafe(format!("DROP INDEX {index}")))
            .execute(&pool)
            .await?;
    }
    sqlx::query(AssertSqlSafe(format!(
        "ALTER TABLE {threads} DROP COLUMN section_position, \
         DROP COLUMN section_entered_at, DROP COLUMN thread_section_id, \
         DROP COLUMN repository_identity"
    )))
    .execute(&pool)
    .await?;
    sqlx::query(AssertSqlSafe(format!(
        "DROP TABLE {}",
        super::qualified_table(fixture.schema(), "thread_sections")
    )))
    .execute(&pool)
    .await?;
    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {migrations} WHERE version >= $1"
    )))
    .bind(REPOSITORY_IDENTITY_MIGRATION_VERSION)
    .execute(&pool)
    .await?;

    let mut expected_rows = Vec::new();
    for (index, (origin, expected)) in repository_identity_cases().iter().enumerate() {
        let thread_id =
            ThreadId::from_string(&format!("00000000-0000-0000-0000-{:012}", index + 81))?;
        let original_projection = serde_json::json!({
            "thread_id": thread_id,
            "preview": format!("canonicalization-case-{index}"),
            "git_info": {
                "repository_url": origin,
            }
        });
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {threads} (thread_id, projection, stream_version, fencing_token, writer_id, \
             writer_lease_expires_at, created_at, updated_at, recency_at) \
             VALUES ($1, $2, 0, 1, 'migration-contract', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, \
             CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
        )))
        .bind(thread_id.to_string())
        .bind(&original_projection)
        .execute(&pool)
        .await?;
        let mut expected_projection = original_projection;
        if let Some(repository_identity) = expected {
            expected_projection["repository_identity"] =
                serde_json::Value::String(repository_identity.to_string());
        }
        expected_rows.push((thread_id.to_string(), expected_projection, expected.clone()));
    }
    pool.close().await;

    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let replica = fixture.connect_pool().await?;
    let rows: Vec<(String, serde_json::Value, Option<String>)> =
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT thread_id, \
             projection - 'section' - 'section_position' - 'section_entered_at', \
             repository_identity FROM {threads} ORDER BY thread_id"
        )))
        .fetch_all(&replica)
        .await?;
    assert_eq!(rows, expected_rows);

    replica.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_external_agent_imports_are_shared_between_replicas() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "external_agent_imports")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let writer = crate::ExternalAgentConfigImportStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    let reader = crate::ExternalAgentConfigImportStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );

    run_external_agent_config_import_contract(&writer, &reader).await?;

    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_stage1_claims_and_outputs_are_shared_between_replicas() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "stage1_claim_output")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let first_pool = fixture.connect_pool().await?;
    let thread_id = ThreadId::new();
    let threads_table = super::qualified_table(fixture.schema(), "threads");
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {threads_table} (thread_id, projection, stream_version, fencing_token, \
         writer_id, writer_lease_expires_at, created_at, updated_at, recency_at) \
         VALUES ($1, '{{}}', 0, 1, 'memory-contract', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, \
         CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
    )))
    .bind(thread_id.to_string())
    .execute(&first_pool)
    .await?;
    let first = crate::MemoryStore::from_postgres(first_pool.clone(), fixture.schema().to_string());
    let second = crate::MemoryStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );

    run_stage1_claim_and_output_contract(&first, &second, thread_id).await?;
    run_stage1_retry_and_lease_contract(&first, &second, thread_id).await?;

    let outputs_table = super::qualified_table(fixture.schema(), "memory_stage1_outputs");
    let jobs_table = super::qualified_table(fixture.schema(), "memory_jobs");
    let before_delete: (i64, i64) = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT (SELECT COUNT(*) FROM {outputs_table} WHERE thread_id = $1), \
         (SELECT COUNT(*) FROM {jobs_table} WHERE kind = 'memory_stage1' AND thread_id = $1)"
    )))
    .bind(thread_id.to_string())
    .fetch_one(&first_pool)
    .await?;
    assert_eq!(before_delete, (1, 1));

    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {threads_table} WHERE thread_id = $1"
    )))
    .bind(thread_id.to_string())
    .execute(&first_pool)
    .await?;
    let after_delete: (i64, i64, i64) = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT (SELECT COUNT(*) FROM {outputs_table} WHERE thread_id = $1), \
         (SELECT COUNT(*) FROM {jobs_table} WHERE kind = 'memory_stage1' AND thread_id = $1), \
         (SELECT COUNT(*) FROM {jobs_table} WHERE kind = 'memory_consolidate_global' \
         AND thread_id IS NULL)"
    )))
    .bind(thread_id.to_string())
    .fetch_one(&first_pool)
    .await?;
    assert_eq!(after_delete, (0, 0, 1));

    first.close().await;
    second.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_stage1_output_data_matches_sqlite() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "stage1_output_data")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    run_postgres_stage1_output_data_contract(
        fixture.connect_pool().await?,
        fixture.connect_pool().await?,
        fixture.schema(),
    )
    .await?;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_stage1_startup_matches_sqlite_across_replicas() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "stage1_startup")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    run_postgres_stage1_startup_contract(
        fixture.connect_pool().await?,
        fixture.connect_pool().await?,
        fixture.schema(),
    )
    .await?;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_phase2_enqueue_and_claim_are_shared_between_replicas() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "phase2_enqueue_claim")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let first = crate::MemoryStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    let second = crate::MemoryStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );

    run_phase2_enqueue_and_claim_contract(&first, &second).await?;
    run_phase2_heartbeat_and_failure_contract(&first, &second).await?;

    first.close().await;
    second.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_phase2_success_matches_sqlite_across_replicas() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "phase2_success")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let thread_ids = phase2_success_thread_ids()?;
    seed_postgres_phase2_success_threads(&first_pool, fixture.schema(), thread_ids).await?;
    let first = crate::MemoryStore::from_postgres(first_pool.clone(), fixture.schema().to_string());
    let second = crate::MemoryStore::from_postgres(second_pool, fixture.schema().to_string());
    let jobs_table = super::qualified_table(fixture.schema(), "memory_jobs");
    let age_pool = first_pool.clone();
    let age_jobs_table = jobs_table.clone();
    let read_pool = first_pool.clone();
    run_phase2_success_contract(
        &first,
        &second,
        thread_ids,
        move || async move {
            sqlx::query(AssertSqlSafe(format!(
                "UPDATE {age_jobs_table} SET finished_at = 0 \
                 WHERE kind = 'memory_consolidate_global' AND job_key = 'global'"
            )))
            .execute(&age_pool)
            .await?;
            Ok(())
        },
        move || async move {
            Ok(sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT COALESCE(last_success_watermark, -1) FROM {jobs_table} \
                 WHERE kind = 'memory_consolidate_global' AND job_key = 'global'"
            )))
            .fetch_one(&read_pool)
            .await?)
        },
    )
    .await?;
    first.close().await;
    second.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_goals_are_shared_between_replicas() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "goals_visibility")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let writer_pool = fixture.connect_pool().await?;
    let thread_id = ThreadId::new();
    let threads_table = super::qualified_table(fixture.schema(), "threads");
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {threads_table} (thread_id, projection, stream_version, fencing_token, \
         writer_id, writer_lease_expires_at, created_at, updated_at, recency_at) \
         VALUES ($1, '{{}}', 0, 1, 'goal-contract', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, \
         CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
    )))
    .bind(thread_id.to_string())
    .execute(&writer_pool)
    .await?;
    let writer = crate::GoalStore::from_postgres(writer_pool.clone(), fixture.schema().to_string());
    let reader = crate::GoalStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );

    run_goal_lifecycle_contract(&writer, &reader, thread_id).await?;

    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {threads_table} WHERE thread_id = $1"
    )))
    .bind(thread_id.to_string())
    .execute(&writer_pool)
    .await?;
    assert_eq!(reader.get_thread_goal(thread_id).await?, None);

    writer.close().await;
    reader.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_goal_errors_match_sqlite() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "goal_errors")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let postgres_pool = fixture.connect_pool().await?;
    let postgres_thread_id = ThreadId::new();
    let threads_table = super::qualified_table(fixture.schema(), "threads");
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {threads_table} (thread_id, projection, stream_version, fencing_token, \
         writer_id, writer_lease_expires_at, created_at, updated_at, recency_at) \
         VALUES ($1, '{{}}', 0, 1, 'goal-error-contract', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, \
         CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
    )))
    .bind(postgres_thread_id.to_string())
    .execute(&postgres_pool)
    .await?;
    let postgres =
        crate::GoalStore::from_postgres(postgres_pool.clone(), fixture.schema().to_string());

    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    let _sqlite_cleanup = scopeguard::guard(sqlite_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let sqlite = crate::StateRuntime::init_sqlite(sqlite_home, "test-provider".to_string()).await?;
    let sqlite_error_thread_id = ThreadId::new();
    let sqlite_error =
        provoke_goal_accounting_conflict(sqlite.thread_goals(), sqlite_error_thread_id).await?;
    let postgres_error = provoke_goal_accounting_conflict(&postgres, postgres_thread_id).await?;

    assert_eq!(postgres_error.kind(), sqlite_error.kind());
    assert_eq!(postgres_error.operation(), sqlite_error.operation());
    assert_eq!(postgres_error.to_string(), sqlite_error.to_string());

    let sqlite_goal = sqlite
        .thread_goals()
        .get_thread_goal(sqlite_error_thread_id)
        .await?
        .expect("SQLite goal should exist before closing the store");
    let postgres_goal = postgres
        .get_thread_goal(postgres_thread_id)
        .await?
        .expect("PostgreSQL goal should exist before closing the store");
    let sqlite_persistence_errors =
        collect_closed_goal_store_errors(sqlite.thread_goals(), &sqlite_goal).await;
    let postgres_persistence_errors =
        collect_closed_goal_store_errors(&postgres, &postgres_goal).await;
    assert_eq!(
        postgres_persistence_errors
            .iter()
            .map(goal_store_error_signature)
            .collect::<Vec<_>>(),
        sqlite_persistence_errors
            .iter()
            .map(goal_store_error_signature)
            .collect::<Vec<_>>()
    );

    sqlite.close().await;
    fixture.cleanup().await
}

async fn postgres_log_replicas(fixture: &PostgresContractFixture) -> Result<(LogStore, LogStore)> {
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let writer_pool = fixture.connect_pool().await?;
    let threads = super::qualified_table(fixture.schema(), "threads");
    for thread_id in ["thread-1", "thread-2", "oversized-thread"] {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {threads} (thread_id, projection, stream_version, fencing_token, \
             writer_id, writer_lease_expires_at, created_at, updated_at, recency_at) \
             VALUES ($1, '{{}}', 0, 1, 'log-contract', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, \
             CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
        )))
        .bind(thread_id)
        .execute(&writer_pool)
        .await?;
    }
    let writer = LogStore::from_postgres(writer_pool, fixture.schema().to_string());
    let reader =
        LogStore::from_postgres(fixture.connect_pool().await?, fixture.schema().to_string());
    Ok((writer, reader))
}

async fn postgres_remote_control_replicas(
    fixture: &PostgresContractFixture,
) -> Result<(RemoteControlEnrollmentStore, RemoteControlEnrollmentStore)> {
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let writer = RemoteControlEnrollmentStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    let reader = RemoteControlEnrollmentStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    Ok((writer, reader))
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_backfill_coordination_is_shared_between_replicas() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "backfill_coordination")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let first_pool = fixture.connect_pool().await?;
    let second_pool = fixture.connect_pool().await?;
    let first =
        crate::BackfillCoordinator::from_postgres(first_pool.clone(), fixture.schema().to_string());
    let second = crate::BackfillCoordinator::from_postgres(
        second_pool.clone(),
        fixture.schema().to_string(),
    );

    run_backfill_coordination_contract(&first, &second).await?;

    first_pool.close().await;
    second_pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_remote_control_is_shared_between_replicas() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "remote_control")?;
    let (writer, reader) = postgres_remote_control_replicas(&fixture).await?;

    run_remote_control_enrollment_contract(&writer, &reader).await?;

    writer.close().await;
    reader.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_remote_control_concurrent_upserts_keep_one_valid_record() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "rc_concurrent")?;
    let (first_replica, second_replica) = postgres_remote_control_replicas(&fixture).await?;
    let first = crate::RemoteControlEnrollmentRecord {
        websocket_url: "wss://example.com/remote/control".to_string(),
        account_id: "shared-account".to_string(),
        app_server_client_name: Some("desktop".to_string()),
        server_id: "server-first".to_string(),
        environment_id: "environment-first".to_string(),
        server_name: "First".to_string(),
        remote_control_enabled: Some(true),
    };
    let second = crate::RemoteControlEnrollmentRecord {
        server_id: "server-second".to_string(),
        environment_id: "environment-second".to_string(),
        server_name: "Second".to_string(),
        ..first.clone()
    };

    let (first_result, second_result) =
        tokio::join!(first_replica.upsert(&first), second_replica.upsert(&second));
    first_result?;
    second_result?;
    let persisted = first_replica
        .get(
            &first.websocket_url,
            &first.account_id,
            first.app_server_client_name.as_deref(),
        )
        .await?
        .expect("one concurrent enrollment must remain");
    assert!([first, second].contains(&persisted));

    first_replica.close().await;
    second_replica.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_remote_control_failures_are_backend_independent_and_sanitized()
-> Result<()> {
    let database_url = test_database_url()?;
    let secret = database_url.clone();
    let mut fixture = PostgresContractFixture::new(database_url, "remote_control_failure")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let store = RemoteControlEnrollmentStore::from_postgres(
        fixture.connect_pool().await?,
        fixture.schema().to_string(),
    );
    store.close().await;

    let error = store
        .get(
            "wss://example.com/remote/control",
            "account",
            /*app_server_client_name*/ None,
        )
        .await
        .expect_err("a closed PostgreSQL pool must fail without fallback");
    let message = error.to_string();
    let rendered = format!("{error:?} {error}");
    assert_eq!(
        message,
        "Runtime State could not complete the `get remote control enrollment` operation; verify enrollment persistence health, then retry"
    );
    assert!(!rendered.contains(&secret));
    assert!(!rendered.contains("SELECT "));
    for backend_term in ["postgres", "sqlite", "sql"] {
        assert!(!message.to_ascii_lowercase().contains(backend_term));
    }

    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_logs_share_single_and_batch_inserts_between_replicas() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "logs_visibility")?;
    let (writer, reader) = postgres_log_replicas(&fixture).await?;

    run_replica_visibility_contract(&writer, &reader).await?;

    writer.close().await;
    reader.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_logs_match_filters_order_and_maximum_id() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "logs_filters")?;
    let (writer, reader) = postgres_log_replicas(&fixture).await?;

    run_filter_order_and_max_id_contract(&writer, &reader).await?;

    writer.close().await;
    reader.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_logs_render_feedback_bodies() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "logs_feedback")?;
    let (writer, reader) = postgres_log_replicas(&fixture).await?;

    run_feedback_contract(&writer, &reader).await?;

    writer.close().await;
    reader.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_logs_apply_startup_retention() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "logs_startup_retention")?;
    let (writer, reader) = postgres_log_replicas(&fixture).await?;

    run_startup_retention_contract(&writer, &reader).await?;

    writer.close().await;
    reader.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_logs_preserve_partition_limits() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "logs_partition_limits")?;
    let (writer, reader) = postgres_log_replicas(&fixture).await?;

    run_partition_limits_contract(&writer, &reader).await?;

    writer.close().await;
    reader.close().await;
    fixture.cleanup().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_log_insert_cannot_escape_concurrent_thread_delete() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "log_delete_race")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    fixture.mark_runtime_ready_for_tests().await?;
    let pool = fixture.connect_pool().await?;
    let threads = super::qualified_table(fixture.schema(), "threads");
    let logs = super::qualified_table(fixture.schema(), "logs");
    let thread_id = ThreadId::new();
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {threads} (thread_id, projection, stream_version, fencing_token, writer_id, \
         writer_lease_expires_at, created_at, updated_at, recency_at) \
         VALUES ($1, '{{}}', 0, 1, 'log-delete-contract', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, \
         CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
    )))
    .bind(thread_id.to_string())
    .execute(&pool)
    .await?;
    let writer_runtime = crate::StateRuntime::init_with_backend(
        crate::RuntimeStateBackendConfig::Postgresql {
            codex_home: AbsolutePathBuf::try_from(crate::runtime::test_support::unique_temp_dir())?,
            namespace: fixture.config_for_tests(),
        },
        "test-provider".to_string(),
    )
    .await?;
    let delete_runtime = crate::StateRuntime::init_with_backend(
        crate::RuntimeStateBackendConfig::Postgresql {
            codex_home: AbsolutePathBuf::try_from(crate::runtime::test_support::unique_temp_dir())?,
            namespace: fixture.config_for_tests(),
        },
        "test-provider".to_string(),
    )
    .await?;

    let mut blocker = pool.begin().await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(\
         'codex-runtime-state:logs:' || $1, 0))",
    )
    .bind(format!("{}:thread:{thread_id}", fixture.schema()))
    .execute(&mut *blocker)
    .await?;
    let entry = crate::LogEntry {
        ts: 1,
        ts_nanos: 0,
        level: "INFO".to_string(),
        target: "log-delete-contract".to_string(),
        message: Some("racing log".to_string()),
        feedback_log_body: None,
        thread_id: Some(thread_id.to_string()),
        process_uuid: None,
        module_path: None,
        file: None,
        line: None,
    };
    let insert_runtime = writer_runtime.clone();
    let mut insert_task = tokio::spawn(async move { insert_runtime.insert_log(&entry).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut insert_task)
            .await
            .is_err(),
        "the test advisory lock must hold the racing insert"
    );

    assert_eq!(delete_runtime.delete_thread(thread_id).await?, 1);
    blocker.commit().await?;
    let insert_error = insert_task
        .await?
        .expect_err("a log cannot be inserted after its canonical thread is deleted");
    assert!(!insert_error.to_string().contains(&thread_id.to_string()));
    let remaining: (i64, i64) = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT (SELECT COUNT(*) FROM {threads} WHERE thread_id = $1), \
         (SELECT COUNT(*) FROM {logs} WHERE thread_id = $1)"
    )))
    .bind(thread_id.to_string())
    .fetch_one(&pool)
    .await?;
    assert_eq!(remaining, (0, 0));

    writer_runtime.close().await;
    delete_runtime.close().await;
    pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_stale_thread_log_does_not_poison_mixed_batch() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "mixed_stale_log_batch")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    fixture.mark_runtime_ready_for_tests().await?;
    let pool = fixture.connect_pool().await?;
    let threads = super::qualified_table(fixture.schema(), "threads");
    let logs = super::qualified_table(fixture.schema(), "logs");
    let active_thread_id = ThreadId::new();
    let deleted_thread_id = ThreadId::new();
    for thread_id in [active_thread_id, deleted_thread_id] {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {threads} (thread_id, projection, stream_version, fencing_token, writer_id, \
             writer_lease_expires_at, created_at, updated_at, recency_at) \
             VALUES ($1, '{{}}', 0, 1, 'mixed-log-contract', CURRENT_TIMESTAMP, \
             CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
        )))
        .bind(thread_id.to_string())
        .execute(&pool)
        .await?;
    }
    let writer_runtime = crate::StateRuntime::init_with_backend(
        crate::RuntimeStateBackendConfig::Postgresql {
            codex_home: AbsolutePathBuf::try_from(crate::runtime::test_support::unique_temp_dir())?,
            namespace: fixture.config_for_tests(),
        },
        "test-provider".to_string(),
    )
    .await?;
    let delete_runtime = crate::StateRuntime::init_with_backend(
        crate::RuntimeStateBackendConfig::Postgresql {
            codex_home: AbsolutePathBuf::try_from(crate::runtime::test_support::unique_temp_dir())?,
            namespace: fixture.config_for_tests(),
        },
        "test-provider".to_string(),
    )
    .await?;
    assert_eq!(delete_runtime.delete_thread(deleted_thread_id).await?, 1);

    let base_entry = crate::LogEntry {
        ts: 1,
        ts_nanos: 0,
        level: "INFO".to_string(),
        target: "mixed-log-contract".to_string(),
        message: Some("stale thread log".to_string()),
        feedback_log_body: None,
        thread_id: Some(deleted_thread_id.to_string()),
        process_uuid: Some("mixed-log-process".to_string()),
        module_path: None,
        file: None,
        line: None,
    };
    let mut active_entry = base_entry.clone();
    active_entry.ts = 2;
    active_entry.message = Some("active thread log".to_string());
    active_entry.thread_id = Some(active_thread_id.to_string());
    let mut process_entry = base_entry.clone();
    process_entry.ts = 3;
    process_entry.message = Some("process log".to_string());
    process_entry.thread_id = None;

    writer_runtime
        .insert_logs(&[base_entry, active_entry, process_entry])
        .await?;
    let counts: (i64, i64, i64) = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT \
         COUNT(*) FILTER (WHERE thread_id = $1), \
         COUNT(*) FILTER (WHERE thread_id = $2), \
         COUNT(*) FILTER (WHERE thread_id IS NULL AND process_uuid = $3) \
         FROM {logs}"
    )))
    .bind(deleted_thread_id.to_string())
    .bind(active_thread_id.to_string())
    .bind("mixed-log-process")
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (0, 1, 1));

    writer_runtime.close().await;
    delete_runtime.close().await;
    pool.close().await;
    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_log_failures_are_sanitized_without_sqlite_fallback() -> Result<()> {
    let database_url = test_database_url()?;
    let secret = database_url.clone();
    let mut fixture = PostgresContractFixture::new(database_url, "logs_failure")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let store =
        LogStore::from_postgres(fixture.connect_pool().await?, fixture.schema().to_string());
    store.close().await;

    let error = store
        .query_logs(&crate::LogQuery::default())
        .await
        .expect_err("a closed PostgreSQL pool must fail without fallback");
    let message = error.to_string();
    let rendered = format!("{error:?} {error}");
    assert_eq!(
        message,
        format!(
            "PostgreSQL could not complete the `query logs` operation for schema `{}`; verify the namespace and server health, then retry",
            fixture.schema()
        )
    );
    assert!(!rendered.contains(&secret));
    assert!(!rendered.contains("SELECT "));
    assert!(!message.to_ascii_lowercase().contains("sqlite"));

    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_creates_migrates_validates_and_cleans_up_namespace() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "lifecycle")?;

    assert!(!fixture.schema_exists().await?);
    let migrated = fixture.manage(PostgresNamespaceAction::Migrate).await?;
    assert!(fixture.schema_exists().await?);
    let validated = fixture.manage(PostgresNamespaceAction::Validate).await?;

    assert_eq!(validated, migrated);
    assert_eq!(migrated.schema(), fixture.schema());
    assert_eq!(migrated.version(), MAXIMUM_COMPATIBLE_SCHEMA_VERSION);

    fixture.cleanup().await?;
    assert!(!fixture.schema_exists().await?);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_migration_is_idempotent() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "idempotent")?;

    let first = fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let second = fixture.manage(PostgresNamespaceAction::Migrate).await?;

    assert_eq!(second, first);
    assert_eq!(
        fixture.migration_history().await?,
        MigrationHistory {
            minimum: Some(1),
            maximum: Some(MAXIMUM_COMPATIBLE_SCHEMA_VERSION),
            count: MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
        }
    );
    fixture.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_validation_is_read_only() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "readonly")?;

    let missing_error = fixture
        .validate_read_only()
        .await
        .expect_err("validation should not create an absent schema");
    assert!(missing_error.to_string().contains("does not exist"));
    assert!(!fixture.schema_exists().await?);

    let migrated = fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let history_before = fixture.migration_history().await?;
    let validated = fixture.validate_read_only().await?;
    let history_after = fixture.migration_history().await?;

    assert_eq!(validated, migrated);
    assert_eq!(history_after, history_before);

    fixture.cleanup().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_connection_rejects_unready_namespace_without_ddl() -> Result<()>
{
    let database_url = test_database_url()?;
    let secret = database_url.clone();
    let mut fixture = PostgresContractFixture::new(database_url, "runtime_unready")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let history_before = fixture.migration_history().await?;
    let pool = fixture.connect_pool().await?;
    let tables_before: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_catalog.pg_tables WHERE schemaname = $1 ORDER BY tablename",
    )
    .bind(fixture.schema())
    .fetch_all(&pool)
    .await?;
    pool.close().await;

    let error = PostgresRuntimeStatePool::connect(fixture.config_for_tests())
        .await
        .err()
        .expect("a schema migration alone must not authorize runtime traffic");

    let pool = fixture.connect_pool().await?;
    let tables_after: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_catalog.pg_tables WHERE schemaname = $1 ORDER BY tablename",
    )
    .bind(fixture.schema())
    .fetch_all(&pool)
    .await?;
    pool.close().await;
    assert_eq!(tables_after, tables_before);
    assert_eq!(fixture.migration_history().await?, history_before);
    let rendered = format!("{error:?} {error}");
    assert!(error.to_string().contains("not ready for runtime use"));
    assert!(error.to_string().contains("codex state migrate"));
    assert!(!rendered.contains(&secret));

    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_runtime_connection_accepts_final_readiness_fence() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "runtime_ready")?;
    fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let pool = fixture.connect_pool().await?;
    let migration = super::qualified_table(fixture.schema(), "runtime_state_migration");
    let evidence = serde_json::json!({
        "sourceIdentity": "test-source",
        "sourceFingerprint": "test-fingerprint",
        "phase": "ready",
        "ready": true,
        "fencingToken": 4,
        "namespaceDigest": "test-final-digest",
        "globalReferentialIntegrityValidated": true,
        "canonicalThreadHistoryOrderingValidated": true,
    });
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {migration} (source_identity, source_fingerprint, phase, ready, \
         phase_evidence, fencing_token) VALUES ($1, $2, 'ready', TRUE, $3, 4)"
    )))
    .bind("test-source")
    .bind("test-fingerprint")
    .bind(evidence)
    .execute(&pool)
    .await?;
    pool.close().await;

    let runtime_pool = PostgresRuntimeStatePool::connect(fixture.config_for_tests()).await?;
    runtime_pool.close().await;

    fixture.cleanup().await
}

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_migration_uses_namespace_advisory_lock() -> Result<()> {
    let database_url = test_database_url()?;
    let mut fixture = PostgresContractFixture::new(database_url, "locking")?;
    let initial = fixture.manage(PostgresNamespaceAction::Migrate).await?;
    let mut held_lock = fixture.hold_migration_lock().await?;
    let mut contending_migration = Box::pin(fixture.manage(PostgresNamespaceAction::Migrate));

    tokio::select! {
        result = &mut contending_migration => {
            anyhow::bail!(
                "migration completed before the namespace advisory lock was released: {result:?}"
            );
        }
        observed = held_lock.wait_for_waiter() => {
            observed.context("observe migration waiting on the namespace advisory lock")?;
        }
    }

    held_lock.release().await?;
    let contending = tokio::time::timeout(Duration::from_secs(5), &mut contending_migration)
        .await
        .context("migration did not resume after the namespace advisory lock was released")??;
    assert_eq!(contending, initial);
    assert_eq!(
        fixture.migration_history().await?,
        MigrationHistory {
            minimum: Some(1),
            maximum: Some(MAXIMUM_COMPATIBLE_SCHEMA_VERSION),
            count: MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
        }
    );
    drop(contending_migration);

    fixture.cleanup().await?;
    Ok(())
}
