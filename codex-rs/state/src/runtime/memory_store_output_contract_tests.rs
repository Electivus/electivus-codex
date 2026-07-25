use super::MemoryStore;
use super::StateRuntime;
use super::test_support::test_thread_metadata;
use super::test_support::unique_temp_dir;
use crate::Stage1Output;
use crate::postgres::qualified_table;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use serde_json::json;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use std::path::PathBuf;

const USED: usize = 0;
const FRESH: usize = 1;
const FRESH_TIE: usize = 2;
const SELECTED_BASELINE: usize = 5;

struct OutputSeed {
    output: Stage1Output,
    usage_count: Option<i64>,
    last_usage: Option<i64>,
    selected_for_phase2: bool,
    enabled: bool,
}

fn contract_seeds() -> Result<Vec<OutputSeed>> {
    let specifications = [
        (
            "0198c4cf-8587-7d32-8d1c-2c14d331f101",
            2_000_000_001,
            None,
            None,
            false,
            true,
        ),
        (
            "0198c4cf-8587-7d32-8d1c-2c14d331f102",
            2_000_000_002,
            None,
            None,
            false,
            true,
        ),
        (
            "0198c4cf-8587-7d32-8d1c-2c14d331f103",
            2_000_000_002,
            None,
            None,
            false,
            true,
        ),
        (
            "0198c4cf-8587-7d32-8d1c-2c14d331f104",
            1_000_000_002,
            None,
            None,
            false,
            true,
        ),
        (
            "0198c4cf-8587-7d32-8d1c-2c14d331f105",
            1_900_000_000,
            Some(5),
            Some(1_000_000_001),
            false,
            true,
        ),
        (
            "0198c4cf-8587-7d32-8d1c-2c14d331f106",
            1_000_000_003,
            None,
            None,
            true,
            true,
        ),
        (
            "0198c4cf-8587-7d32-8d1c-2c14d331f107",
            2_100_000_000,
            None,
            None,
            false,
            false,
        ),
    ];
    specifications
        .into_iter()
        .enumerate()
        .map(
            |(
                index,
                (
                    thread_id,
                    source_updated_at,
                    usage_count,
                    last_usage,
                    selected_for_phase2,
                    enabled,
                ),
            )| {
                Ok(OutputSeed {
                    output: Stage1Output {
                        thread_id: ThreadId::from_string(thread_id)?,
                        rollout_path: PathBuf::from(format!(
                            "/contract/rollouts/output-{index}.jsonl"
                        )),
                        source_updated_at: timestamp(source_updated_at)?,
                        raw_memory: format!("raw memory {index}"),
                        rollout_summary: format!("rollout summary {index}"),
                        rollout_slug: (index % 2 == 0).then(|| format!("output-{index}")),
                        cwd: PathBuf::from(format!("/contract/workspaces/workspace-{index}")),
                        git_branch: Some(format!("contract/branch-{index}")),
                        generated_at: timestamp(source_updated_at + 10)?,
                    },
                    usage_count,
                    last_usage,
                    selected_for_phase2,
                    enabled,
                })
            },
        )
        .collect()
}

fn timestamp(seconds: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp(seconds, /*nsecs*/ 0)
        .ok_or_else(|| anyhow::anyhow!("invalid contract timestamp: {seconds}"))
}

async fn run_stage1_output_data_contract(
    writer: &MemoryStore,
    reader: &MemoryStore,
    seeds: &[OutputSeed],
) -> Result<()> {
    assert_eq!(
        reader.list_stage1_outputs_for_global(/*n*/ 0).await?,
        Vec::<Stage1Output>::new()
    );
    assert_eq!(
        reader.list_stage1_outputs_for_global(/*n*/ 2).await?,
        vec![seeds[FRESH_TIE].output.clone(), seeds[FRESH].output.clone()]
    );

    let missing = ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f108")?;
    assert_eq!(
        writer
            .record_stage1_output_usage(&[
                seeds[USED].output.thread_id,
                seeds[USED].output.thread_id,
                seeds[FRESH].output.thread_id,
                missing,
            ])
            .await?,
        3
    );
    let expected_candidates = vec![
        seeds[USED].output.clone(),
        seeds[FRESH].output.clone(),
        seeds[FRESH_TIE].output.clone(),
    ];
    assert_eq!(
        reader
            .get_phase2_input_selection(/*n*/ 10, /*max_unused_days*/ 30)
            .await?,
        expected_candidates
    );

    assert_eq!(
        writer
            .prune_stage1_outputs_for_retention(/*max_unused_days*/ 30, /*limit*/ 1)
            .await?,
        1
    );
    assert_eq!(
        writer
            .prune_stage1_outputs_for_retention(/*max_unused_days*/ 30, /*limit*/ 100)
            .await?,
        1
    );
    assert_eq!(
        writer
            .prune_stage1_outputs_for_retention(/*max_unused_days*/ 30, /*limit*/ 100)
            .await?,
        0
    );
    assert_eq!(
        reader.list_stage1_outputs_for_global(/*n*/ 100).await?,
        vec![
            seeds[FRESH_TIE].output.clone(),
            seeds[FRESH].output.clone(),
            seeds[USED].output.clone(),
            seeds[SELECTED_BASELINE].output.clone(),
        ]
    );
    assert_eq!(
        reader
            .get_phase2_input_selection(/*n*/ 10, /*max_unused_days*/ 30)
            .await?,
        expected_candidates
    );
    Ok(())
}

async fn seed_sqlite(runtime: &StateRuntime, seeds: &[OutputSeed]) -> Result<()> {
    for seed in seeds {
        let mut metadata = test_thread_metadata(
            runtime.sqlite().home(),
            seed.output.thread_id,
            seed.output.cwd.clone(),
        );
        metadata.rollout_path = seed.output.rollout_path.clone();
        metadata.git_branch = seed.output.git_branch.clone();
        runtime.upsert_thread(&metadata).await?;
        if !seed.enabled {
            runtime
                .set_thread_memory_mode(seed.output.thread_id, "polluted")
                .await?;
        }
        sqlx::query(
            "INSERT INTO stage1_outputs (thread_id, source_updated_at, raw_memory, \
             rollout_summary, rollout_slug, generated_at, usage_count, last_usage, \
             selected_for_phase2, selected_for_phase2_source_updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(seed.output.thread_id.to_string())
        .bind(seed.output.source_updated_at.timestamp())
        .bind(&seed.output.raw_memory)
        .bind(&seed.output.rollout_summary)
        .bind(&seed.output.rollout_slug)
        .bind(seed.output.generated_at.timestamp())
        .bind(seed.usage_count)
        .bind(seed.last_usage)
        .bind(seed.selected_for_phase2)
        .bind(
            seed.selected_for_phase2
                .then_some(seed.output.source_updated_at.timestamp()),
        )
        .execute(runtime.memories().sqlite_pool_for_tests())
        .await?;
    }
    Ok(())
}

pub(crate) async fn run_postgres_stage1_output_data_contract(
    writer_pool: PgPool,
    reader_pool: PgPool,
    schema: &str,
) -> Result<()> {
    let seeds = contract_seeds()?;
    seed_postgres(&writer_pool, schema, &seeds).await?;
    let writer = MemoryStore::from_postgres(writer_pool, schema.to_string());
    let reader = MemoryStore::from_postgres(reader_pool, schema.to_string());
    run_stage1_output_data_contract(&writer, &reader, &seeds).await?;
    writer.close().await;
    reader.close().await;
    Ok(())
}

async fn seed_postgres(pool: &PgPool, schema: &str, seeds: &[OutputSeed]) -> Result<()> {
    let threads_table = qualified_table(schema, "threads");
    let history_table = qualified_table(schema, "thread_history");
    let outputs_table = qualified_table(schema, "memory_stage1_outputs");
    for seed in seeds {
        let projection = json!({
            "rollout_path": seed.output.rollout_path,
            "cwd": seed.output.cwd,
            "git_info": { "branch": seed.output.git_branch },
        });
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {threads_table} (thread_id, projection, stream_version, fencing_token, \
             writer_id, writer_lease_expires_at, created_at, updated_at, recency_at) \
             VALUES ($1, $2, 0, 1, 'memory-output-contract', CURRENT_TIMESTAMP, \
             CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
        )))
        .bind(seed.output.thread_id.to_string())
        .bind(projection)
        .execute(pool)
        .await?;
        let memory_mode = if seed.enabled { "enabled" } else { "polluted" };
        let foreign_memory_mode = if seed.enabled { "polluted" } else { "enabled" };
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {history_table} (thread_id, ordinal, item) \
             VALUES ($1, 0, $2), ($1, 1, $3)"
        )))
        .bind(seed.output.thread_id.to_string())
        .bind(json!({
            "type": "session_meta",
            "payload": {
                "id": seed.output.thread_id,
                "memory_mode": memory_mode,
            },
        }))
        .bind(json!({
            "type": "session_meta",
            "payload": {
                "id": "0198c4cf-8587-7d32-8d1c-2c14d331fff",
                "memory_mode": foreign_memory_mode,
            },
        }))
        .execute(pool)
        .await?;
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {outputs_table} (thread_id, source_updated_at, raw_memory, \
             rollout_summary, rollout_slug, generated_at, usage_count, last_usage, \
             selected_for_phase2, selected_for_phase2_source_updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"
        )))
        .bind(seed.output.thread_id.to_string())
        .bind(seed.output.source_updated_at.timestamp())
        .bind(&seed.output.raw_memory)
        .bind(&seed.output.rollout_summary)
        .bind(&seed.output.rollout_slug)
        .bind(seed.output.generated_at.timestamp())
        .bind(seed.usage_count)
        .bind(seed.last_usage)
        .bind(seed.selected_for_phase2)
        .bind(
            seed.selected_for_phase2
                .then_some(seed.output.source_updated_at.timestamp()),
        )
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_stage1_output_data_satisfies_shared_contract() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let writer = StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string()).await?;
    let reader = StateRuntime::init_sqlite(codex_home, "test-provider".to_string()).await?;
    let seeds = contract_seeds()?;
    seed_sqlite(&writer, &seeds).await?;

    run_stage1_output_data_contract(writer.memories(), reader.memories(), &seeds).await?;

    writer.close().await;
    reader.close().await;
    Ok(())
}
