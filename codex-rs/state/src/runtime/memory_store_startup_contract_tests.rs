use super::MemoryStore;
use super::StateRuntime;
use super::test_support::test_thread_metadata;
use super::test_support::unique_temp_dir;
use crate::Stage1JobClaim;
use crate::Stage1JobClaimOutcome;
use crate::Stage1StartupClaimParams;
use crate::ThreadMetadata;
use crate::postgres::qualified_table;
use anyhow::Result;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use pretty_assertions::assert_eq;
use serde_json::json;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use std::path::Path;
use std::path::PathBuf;

const CURRENT: usize = 0;
const NEWEST: usize = 1;
const OLDER: usize = 2;
const TOO_FRESH: usize = 4;
const POLLUTED: usize = 7;

struct StartupSeed {
    metadata: ThreadMetadata,
    memory_mode: &'static str,
}

fn startup_seeds(now: DateTime<Utc>, root: &Path) -> Result<Vec<StartupSeed>> {
    let specifications = [
        ("0198c4cf-8587-7d32-8d1c-2c14d3320010", 2, "cli", "enabled"),
        ("0198c4cf-8587-7d32-8d1c-2c14d3320011", 2, "cli", "enabled"),
        ("0198c4cf-8587-7d32-8d1c-2c14d3320012", 3, "cli", "enabled"),
        ("0198c4cf-8587-7d32-8d1c-2c14d3320013", 4, "exec", "enabled"),
        ("0198c4cf-8587-7d32-8d1c-2c14d3320014", 0, "cli", "enabled"),
        (
            "0198c4cf-8587-7d32-8d1c-2c14d3320015",
            24 * 31,
            "cli",
            "enabled",
        ),
        ("0198c4cf-8587-7d32-8d1c-2c14d3320016", 5, "cli", "disabled"),
        ("0198c4cf-8587-7d32-8d1c-2c14d3320017", 6, "cli", "enabled"),
    ];
    specifications
        .into_iter()
        .enumerate()
        .map(|(index, (thread_id, idle_hours, source, memory_mode))| {
            let thread_id = ThreadId::from_string(thread_id)?;
            let mut metadata = test_thread_metadata(
                root,
                thread_id,
                PathBuf::from(format!("/contract/workspaces/startup-{index}")),
            );
            metadata.rollout_path = PathBuf::from(format!(
                "/contract/rollouts/startup-{index}-{thread_id}.jsonl"
            ));
            metadata.created_at = now - Duration::days(40);
            metadata.updated_at = if index == TOO_FRESH {
                now - Duration::minutes(30)
            } else {
                now - Duration::hours(idle_hours)
            };
            metadata.recency_at = metadata.updated_at;
            metadata.source = source.to_string();
            metadata.history_mode = ThreadHistoryMode::Paginated;
            metadata.thread_source = Some(ThreadSource::User);
            metadata.agent_nickname = Some(format!("startup-{index}"));
            metadata.agent_role = Some("worker".to_string());
            metadata.agent_path = Some(format!("/contract/agents/{index}"));
            metadata.model_provider = "startup-provider".to_string();
            metadata.model = Some("gpt-5.4".to_string());
            metadata.reasoning_effort = Some(ReasoningEffort::High);
            metadata.cli_version = "1.2.3".to_string();
            let preview = format!("startup preview {index}");
            metadata.title = preview.clone();
            metadata.name = Some(format!("startup name {index}"));
            metadata.preview = Some(preview.clone());
            metadata.sandbox_policy = serde_json::to_string(&PermissionProfile::read_only())?;
            metadata.tokens_used = 100 + i64::try_from(index)?;
            metadata.first_user_message = Some(preview);
            metadata.git_sha = Some(format!("sha-{index}"));
            metadata.git_branch = Some(format!("startup/branch-{index}"));
            metadata.git_origin_url = Some(format!("https://example.test/{index}.git"));
            Ok(StartupSeed {
                metadata,
                memory_mode,
            })
        })
        .collect()
}

fn startup_params<'a>(allowed_sources: &'a [String]) -> Stage1StartupClaimParams<'a> {
    Stage1StartupClaimParams {
        scan_limit: 20,
        max_claimed: 10,
        max_age_days: 30,
        min_rollout_idle_hours: 1,
        allowed_sources,
        lease_seconds: 60,
    }
}

fn claimed_thread(claims: &[Stage1JobClaim]) -> Vec<ThreadMetadata> {
    claims.iter().map(|claim| claim.thread.clone()).collect()
}

async fn run_stage1_startup_contract(
    writer: &MemoryStore,
    reader: &MemoryStore,
    seeds: &[StartupSeed],
) -> Result<()> {
    assert!(
        writer
            .mark_thread_memory_mode_polluted(seeds[POLLUTED].metadata.id)
            .await?
    );
    assert!(
        !reader
            .mark_thread_memory_mode_polluted(seeds[POLLUTED].metadata.id)
            .await?
    );

    let allowed_sources = ["cli".to_string()];
    let mut zero_scan = startup_params(&allowed_sources);
    zero_scan.scan_limit = 0;
    assert_eq!(
        writer
            .claim_stage1_jobs_for_startup(seeds[CURRENT].metadata.id, zero_scan)
            .await?,
        Vec::<Stage1JobClaim>::new()
    );
    let mut one_claim = startup_params(&allowed_sources);
    one_claim.scan_limit = 1;
    one_claim.max_claimed = 1;
    let newest_claims = writer
        .claim_stage1_jobs_for_startup(seeds[CURRENT].metadata.id, one_claim)
        .await?;
    assert_eq!(
        claimed_thread(&newest_claims),
        vec![seeds[NEWEST].metadata.clone()]
    );
    assert!(
        writer
            .mark_stage1_job_succeeded_no_output(
                seeds[NEWEST].metadata.id,
                &newest_claims[0].ownership_token,
            )
            .await?
    );

    let first_claims = writer.claim_stage1_jobs_for_startup(
        seeds[CURRENT].metadata.id,
        startup_params(&allowed_sources),
    );
    let second_claims = reader.claim_stage1_jobs_for_startup(
        seeds[CURRENT].metadata.id,
        startup_params(&allowed_sources),
    );
    let (first_claims, second_claims) = tokio::join!(first_claims, second_claims);
    let mut concurrent_threads = claimed_thread(&first_claims?);
    concurrent_threads.extend(claimed_thread(&second_claims?));
    assert_eq!(concurrent_threads, vec![seeds[OLDER].metadata.clone()]);

    writer.clear_memory_data().await?;
    let reset_claims = reader
        .claim_stage1_jobs_for_startup(seeds[CURRENT].metadata.id, startup_params(&allowed_sources))
        .await?;
    assert_eq!(
        claimed_thread(&reset_claims),
        vec![
            seeds[NEWEST].metadata.clone(),
            seeds[OLDER].metadata.clone(),
        ]
    );

    let newest = reset_claims
        .iter()
        .find(|claim| claim.thread.id == seeds[NEWEST].metadata.id)
        .expect("newest eligible thread should be claimed after reset");
    assert!(
        writer
            .mark_stage1_job_succeeded(
                newest.thread.id,
                &newest.ownership_token,
                newest.thread.updated_at.timestamp(),
                "startup raw memory",
                "startup summary",
                Some("startup-delete"),
            )
            .await?
    );
    writer.delete_thread_memory(newest.thread.id).await?;
    assert!(matches!(
        reader
            .try_claim_stage1_job(
                newest.thread.id,
                seeds[CURRENT].metadata.id,
                newest.thread.updated_at.timestamp(),
                /*lease_seconds*/ 60,
                /*max_running_jobs*/ 10,
            )
            .await?,
        Stage1JobClaimOutcome::Claimed { .. }
    ));
    Ok(())
}

async fn seed_sqlite(runtime: &StateRuntime, seeds: &[StartupSeed]) -> Result<()> {
    for seed in seeds {
        runtime.upsert_thread(&seed.metadata).await?;
        runtime
            .set_thread_memory_mode(seed.metadata.id, seed.memory_mode)
            .await?;
        sqlx::query(
            "UPDATE threads SET created_at = ?, updated_at = ?, recency_at = ?, \
             created_at_ms = ?, updated_at_ms = ?, recency_at_ms = ? WHERE id = ?",
        )
        .bind(seed.metadata.created_at.timestamp())
        .bind(seed.metadata.updated_at.timestamp())
        .bind(seed.metadata.recency_at.timestamp())
        .bind(seed.metadata.created_at.timestamp_millis())
        .bind(seed.metadata.updated_at.timestamp_millis())
        .bind(seed.metadata.recency_at.timestamp_millis())
        .bind(seed.metadata.id.to_string())
        .execute(runtime.sqlite_pool().expect("SQLite runtime"))
        .await?;
    }
    Ok(())
}

async fn seed_postgres(pool: &PgPool, schema: &str, seeds: &[StartupSeed]) -> Result<()> {
    let threads_table = qualified_table(schema, "threads");
    let history_table = qualified_table(schema, "thread_history");
    for seed in seeds {
        let metadata = &seed.metadata;
        let permission_profile =
            serde_json::from_str::<serde_json::Value>(&metadata.sandbox_policy)?;
        let projection = json!({
            "thread_id": metadata.id,
            "extra_config": null,
            "rollout_path": metadata.rollout_path,
            "forked_from_id": null,
            "parent_thread_id": null,
            "preview": metadata.preview.clone().unwrap_or_default(),
            "name": metadata.name,
            "model_provider": metadata.model_provider,
            "model": metadata.model,
            "reasoning_effort": metadata.reasoning_effort,
            "created_at": metadata.created_at,
            "updated_at": metadata.updated_at,
            "recency_at": metadata.recency_at,
            "archived_at": metadata.archived_at,
            "cwd": metadata.cwd,
            "cli_version": metadata.cli_version,
            "source": metadata.source,
            "history_mode": metadata.history_mode,
            "thread_source": metadata.thread_source,
            "agent_nickname": metadata.agent_nickname,
            "agent_role": metadata.agent_role,
            "agent_path": metadata.agent_path,
            "git_info": {
                "commit_hash": metadata.git_sha,
                "branch": metadata.git_branch,
                "repository_url": metadata.git_origin_url,
            },
            "approval_mode": metadata.approval_mode,
            "permission_profile": permission_profile,
            "token_usage": {
                "input_tokens": 0,
                "cached_input_tokens": 0,
                "cache_write_input_tokens": 0,
                "output_tokens": 0,
                "reasoning_output_tokens": 0,
                "total_tokens": metadata.tokens_used,
            },
            "first_user_message": metadata.first_user_message,
            "history": null,
        });
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {threads_table} (thread_id, projection, stream_version, fencing_token, \
             writer_id, writer_lease_expires_at, created_at, updated_at, recency_at) \
             VALUES ($1, $2, 1, 1, 'startup-contract', CURRENT_TIMESTAMP, $3, $4, $5)"
        )))
        .bind(metadata.id.to_string())
        .bind(projection)
        .bind(metadata.created_at)
        .bind(metadata.updated_at)
        .bind(metadata.recency_at)
        .execute(pool)
        .await?;
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {history_table} (thread_id, ordinal, item) VALUES ($1, 0, $2)"
        )))
        .bind(metadata.id.to_string())
        .bind(json!({
            "type": "session_meta",
            "payload": {
                "id": metadata.id,
                "memory_mode": seed.memory_mode,
            },
        }))
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub(crate) async fn run_postgres_stage1_startup_contract(
    writer_pool: PgPool,
    reader_pool: PgPool,
    schema: &str,
) -> Result<()> {
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT date_trunc('second', clock_timestamp())")
        .fetch_one(&writer_pool)
        .await?;
    let seeds = startup_seeds(now, Path::new("/contract"))?;
    seed_postgres(&writer_pool, schema, &seeds).await?;
    let writer = MemoryStore::from_postgres(writer_pool, schema.to_string());
    let reader = MemoryStore::from_postgres(reader_pool, schema.to_string());
    run_stage1_startup_contract(&writer, &reader, &seeds).await?;
    writer.close().await;
    reader.close().await;
    Ok(())
}

#[tokio::test]
async fn sqlite_stage1_startup_satisfies_shared_contract() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let writer =
        StateRuntime::init_sqlite(codex_home.clone(), "startup-provider".to_string()).await?;
    let reader =
        StateRuntime::init_sqlite(codex_home.clone(), "startup-provider".to_string()).await?;
    let now = DateTime::from_timestamp(
        sqlx::query_scalar::<_, i64>("SELECT CAST(strftime('%s', 'now') AS INTEGER)")
            .fetch_one(writer.sqlite_pool().expect("SQLite runtime"))
            .await?,
        /*nsecs*/ 0,
    )
    .expect("SQLite clock should return a valid timestamp");
    let seeds = startup_seeds(now, codex_home.as_path())?;
    seed_sqlite(&writer, &seeds).await?;
    run_stage1_startup_contract(writer.memories(), reader.memories(), &seeds).await?;
    writer.close().await;
    reader.close().await;
    Ok(())
}
