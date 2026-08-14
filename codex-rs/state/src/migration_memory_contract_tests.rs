use super::CanonicalThreadHistoryReader;
use super::RuntimeStateMigrationPhase;
use super::RuntimeStateThreadProjectionMaterializer;
use super::import_runtime_state_memory;
use super::import_runtime_state_operational;
use super::import_runtime_state_threads;
use super::preflight_runtime_state_migration;
use super::test_support;
use crate::MemoryArtifact;
use crate::MemoryArtifactSet;
use crate::MemoryWorkspaceMaterialization;
use crate::Phase2JobClaimOutcome;
use crate::PostgresNamespaceAction;
use crate::PostgresRuntimeStatePool;
use crate::SqliteConfig;
use crate::postgres::qualified_table;
use crate::postgres::test_support::PostgresContractFixture;
use crate::postgres::test_support::test_database_url;
use crate::runtime::test_support::test_thread_metadata;
use anyhow::Context;
use codex_history::RolloutItem;
use codex_history::RolloutLine;
use codex_protocol::ThreadId;
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

const ARTIFACTS: &[(&str, &[u8])] = &[
    ("MEMORY.md", b"# Migrated memory\n"),
    (
        "extensions/external_agent_import/project/resources/bin.dat",
        &[0, 1, 2, 255],
    ),
    (
        "extensions/tool/cache.json.zst",
        &[0x28, 0xb5, 0x2f, 0xfd, 0, 1, 0xff],
    ),
    ("extensions/tool/index.json", b"{\"enabled\":true}\n"),
    ("memory_summary.md", b"v1\n\nMigration summary.\n"),
    ("rollout_summaries/thread.md", b"Rollout summary.\n"),
    (
        "skills/migration/SKILL.md",
        b"---\nname: migration\n---\nKeep exact bytes.\n",
    ),
];

struct FixtureHistoryReader {
    lines: Vec<RolloutLine>,
}

impl CanonicalThreadHistoryReader for FixtureHistoryReader {
    async fn read(
        &self,
        _path: &Path,
        _maximum_source_bytes: u64,
    ) -> anyhow::Result<(Vec<RolloutLine>, usize, u64)> {
        Ok((self.lines.clone(), 0, 1))
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
async fn postgres_contract_imports_complete_memory_generation_read_only() -> anyhow::Result<()> {
    let (source, runtime) = test_support::initialized_runtime_source("memory").await?;
    let _cleanup = scopeguard::guard(source.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let thread_id = ThreadId::from_string("019c84d0-4444-7777-8444-444444444444")?;
    let worker_id = ThreadId::from_string("019c84d0-5555-7777-8555-555555555555")?;
    let rollout_path = source.join("sessions/2026/07/22/memory.jsonl");
    std::fs::create_dir_all(rollout_path.parent().context("rollout parent")?)?;
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
    let source_updated_at = metadata.updated_at.timestamp();
    let crate::Stage1JobClaimOutcome::Claimed { ownership_token } = runtime
        .memories()
        .try_claim_stage1_job(
            thread_id,
            worker_id,
            source_updated_at,
            /*lease_seconds*/ 60,
            /*max_running_jobs*/ 1,
        )
        .await?
    else {
        anyhow::bail!("stage-one fixture was not claimed");
    };
    assert!(
        runtime
            .memories()
            .mark_stage1_job_succeeded(
                thread_id,
                &ownership_token,
                source_updated_at,
                "raw memory",
                "rollout summary",
                Some("migration-rollout"),
            )
            .await?
    );
    assert_eq!(
        runtime
            .memories()
            .record_stage1_output_usage(&[thread_id, thread_id])
            .await?,
        2
    );
    let expected_outputs = runtime
        .memories()
        .list_stage1_outputs_for_global(/*n*/ 10)
        .await?;
    let mut expected_imported_outputs = expected_outputs.clone();
    for output in &mut expected_imported_outputs {
        output.rollout_path = std::path::PathBuf::new();
    }
    runtime
        .memories()
        .enqueue_global_consolidation(/*input_watermark*/ 42)
        .await?;
    let Phase2JobClaimOutcome::Claimed {
        ownership_token,
        input_watermark,
    } = runtime
        .memories()
        .try_claim_global_phase2_job(worker_id, /*lease_seconds*/ 60)
        .await?
    else {
        anyhow::bail!("phase-two fixture was not claimed");
    };
    assert!(
        runtime
            .memories()
            .mark_global_phase2_job_succeeded(&ownership_token, input_watermark, &expected_outputs,)
            .await?
    );
    for (relative_path, contents) in ARTIFACTS {
        let path = source.join("memories").join(relative_path);
        std::fs::create_dir_all(path.parent().context("artifact parent")?)?;
        std::fs::write(path, contents)?;
    }
    std::fs::create_dir_all(source.join("memories/.git"))?;
    std::fs::write(source.join("memories/.git/HEAD"), b"ref: refs/heads/main\n")?;
    std::fs::write(
        source.join("memories/phase2_workspace_diff.md"),
        b"transient workspace diff\n",
    )?;
    runtime.close().await;

    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(source.as_path())?);
    let memory_pool = sqlite
        .open_immutable_pool(&sqlite.memories_db_path())
        .await?;
    let expected_jobs = sqlite_jobs(&memory_pool).await?;
    let expected_usage: (Option<i64>, Option<i64>, bool, Option<i64>) = sqlx::query_as(
        "SELECT usage_count, last_usage, selected_for_phase2 != 0, \
         selected_for_phase2_source_updated_at FROM stage1_outputs WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(&memory_pool)
    .await?;
    memory_pool.close().await;
    let source_before = test_support::snapshot_source(&source)?;
    let mut destination = PostgresContractFixture::new(test_database_url()?, "memory_migration")?;
    destination.manage(PostgresNamespaceAction::Migrate).await?;
    let config = destination.config_for_tests();
    let inventory = preflight_runtime_state_migration(sqlite.clone(), config.clone()).await?;
    let materializer = EmptyProjectionMaterializer {
        schema: destination.schema().to_string(),
    };
    import_runtime_state_threads(
        &sqlite,
        &config,
        &inventory,
        &FixtureHistoryReader { lines: history },
        &materializer,
    )
    .await?;
    let error = import_runtime_state_memory(&sqlite, &config, &inventory)
        .await
        .expect_err("memory import must reject a missing operational predecessor");
    assert!(
        error
            .to_string()
            .contains("must import operational state before memory")
    );
    import_runtime_state_operational(&sqlite, &config, &inventory).await?;
    let progress = import_runtime_state_memory(&sqlite, &config, &inventory).await?;
    assert_eq!(
        (progress.phase(), progress.fencing_token()),
        (RuntimeStateMigrationPhase::MemoryImported, 3)
    );
    assert_eq!(
        import_runtime_state_memory(&sqlite, &config, &inventory).await?,
        progress
    );

    let pool = PostgresRuntimeStatePool::connect_for_migration(config).await?;
    let store = pool.memory_store();
    assert_eq!(
        store.list_stage1_outputs_for_global(/*n*/ 10).await?,
        expected_imported_outputs
    );
    let generation = store
        .load_active_memory_generation()
        .await?
        .context("migration must publish one active generation")?;
    let expected_artifacts = MemoryArtifactSet::new(
        ARTIFACTS
            .iter()
            .map(|(path, contents)| MemoryArtifact::new(*path, contents.to_vec()))
            .collect::<anyhow::Result<Vec<_>>>()?,
    )?;
    assert_eq!(
        (generation.completed_watermark(), generation.artifacts()),
        (input_watermark, expected_artifacts.artifacts())
    );
    assert_eq!(
        store.memory_workspace_materialization().await?,
        MemoryWorkspaceMaterialization::Replace {
            generation_id: generation.generation_id().to_string(),
            artifacts: expected_artifacts,
        }
    );
    let (raw_pool, schema) = pool.thread_store_connection();
    assert_eq!(postgres_jobs(&raw_pool, &schema).await?, expected_jobs);
    let outputs = qualified_table(&schema, "memory_stage1_outputs");
    let actual_usage: (Option<i64>, Option<i64>, bool, Option<i64>) =
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT usage_count, last_usage, selected_for_phase2, \
         selected_for_phase2_source_updated_at FROM {outputs} WHERE thread_id = $1"
        )))
        .bind(thread_id.to_string())
        .fetch_one(&raw_pool)
        .await?;
    assert_eq!(actual_usage, expected_usage);

    let migration = qualified_table(&schema, "runtime_state_migration");
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT phase, ready, fencing_token, phase_evidence FROM {migration} WHERE singleton"
    )))
    .fetch_one(&raw_pool)
    .await?;
    assert_eq!(
        (
            row.try_get::<String, _>("phase")?,
            row.try_get::<bool, _>("ready")?,
            row.try_get::<i64, _>("fencing_token")?,
        ),
        ("memory_imported".to_string(), false, 3)
    );
    let evidence: Value = row.try_get("phase_evidence")?;
    assert_eq!(
        evidence["memoryArtifactSetHash"],
        Value::from("61c70acbc1763c580d7142cff2e93eedc901b97511e48fd2e3921b6258b34519")
    );
    assert_eq!(
        (
            evidence["memoryStage1OutputsHash"].as_str().map(str::len),
            evidence["memoryJobsHash"].as_str().map(str::len),
        ),
        (Some(64), Some(64))
    );
    assert_eq!(
        (
            &evidence["memoryStage1Outputs"],
            &evidence["memoryJobs"],
            &evidence["memoryUsedOutputs"],
            &evidence["memorySelectedOutputs"],
            &evidence["memoryGenerations"],
            &evidence["memoryArtifacts"],
            &evidence["memoryArtifactBytes"],
        ),
        (
            &Value::from(1),
            &Value::from(2),
            &Value::from(1),
            &Value::from(1),
            &Value::from(1),
            &Value::from(7),
            &Value::from(128),
        )
    );
    assert_eq!(test_support::snapshot_source(&source)?, source_before);

    store.close().await;
    pool.close().await;
    destination.cleanup().await
}

async fn sqlite_jobs(pool: &sqlx::SqlitePool) -> anyhow::Result<Value> {
    let value: String = sqlx::query_scalar(
        "SELECT COALESCE(json_group_array(json(record)), json('[]')) FROM ( \
         SELECT json_object('kind', kind, 'job_key', job_key, 'status', status, \
         'worker_id', worker_id, 'ownership_token', ownership_token, 'started_at', started_at, \
         'finished_at', finished_at, 'lease_until', lease_until, 'retry_at', retry_at, \
         'retry_remaining', retry_remaining, 'last_error', last_error, \
         'input_watermark', input_watermark, 'last_success_watermark', last_success_watermark) \
         AS record FROM jobs ORDER BY kind, job_key) ordered_jobs",
    )
    .fetch_one(pool)
    .await?;
    Ok(serde_json::from_str(&value)?)
}

async fn postgres_jobs(pool: &sqlx::PgPool, schema: &str) -> anyhow::Result<Value> {
    let jobs = qualified_table(schema, "memory_jobs");
    sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COALESCE(jsonb_agg(to_jsonb(job) - 'thread_id' ORDER BY kind, job_key), \
         '[]'::jsonb) FROM {jobs} AS job"
    )))
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}
