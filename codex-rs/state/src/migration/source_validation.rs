use super::SourceFileInventory;
use crate::SqliteConfig;
use anyhow::Context;
use std::collections::HashSet;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

const STATE_TABLES: &str = "_sqlx_migrations,backfill_state,external_agent_config_imports,remote_control_enrollments,thread_dynamic_tools,thread_spawn_edges,threads";
const GOALS_TABLES: &str = "_sqlx_migrations,thread_goal_accounting_events,thread_goal_continuation_deferrals,thread_goals";
const THREAD_HISTORY_TABLES: &str =
    "_sqlx_migrations,thread_history_projection_state,thread_items,thread_turns";
const MAX_ROLLOUT_VALIDATION_BYTES: u64 = 256 * 1024 * 1024;

pub(super) async fn validate_database_schema(
    label: &str,
    pool: &sqlx::SqlitePool,
    tables: &[String],
) -> anyhow::Result<()> {
    let (version, required_tables) = match label {
        "state DB" => (45, STATE_TABLES),
        "log DB" => (2, "_sqlx_migrations,logs"),
        "goals DB" => (3, GOALS_TABLES),
        "memories DB" => (1, "_sqlx_migrations,jobs,stage1_outputs"),
        "thread history DB" => (4, THREAD_HISTORY_TABLES),
        _ => anyhow::bail!("unknown mandatory SQLite database `{label}`"),
    };
    anyhow::ensure!(
        required_tables
            .split(',')
            .all(|required| tables.iter().any(|table| table == required)),
        "SQLite {label} has an incompatible schema; restore a current, complete source database before retrying"
    );
    let current_version: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await?;
    anyhow::ensure!(
        current_version == version,
        "SQLite {label} is at schema version {current_version}, but migration preflight requires version {version}; use a matching Codex version or restore a compatible backup"
    );
    if label == "state DB" {
        let rollout_path_is_required: bool = sqlx::query_scalar(
            "SELECT COUNT(*) = 1 FROM pragma_table_info('threads') \
             WHERE name = 'rollout_path' AND type = 'TEXT' AND \"notnull\" = 1",
        )
        .fetch_one(pool)
        .await?;
        anyhow::ensure!(
            rollout_path_is_required,
            "SQLite state DB has an incompatible threads.rollout_path column; expected required TEXT"
        );
    }
    Ok(())
}

pub(super) async fn validate_rollout_files(
    source: &SqliteConfig,
    rollout_files: &[SourceFileInventory],
) -> anyhow::Result<()> {
    let inventoried_paths = rollout_files
        .iter()
        .filter_map(|file| logical_rollout_path(&file.relative_path))
        .collect::<HashSet<_>>();
    for rollout_path in referenced_rollout_paths(source).await? {
        let relative_path = relative_rollout_path(source.home(), &rollout_path)?;
        anyhow::ensure!(
            inventoried_paths.contains(&relative_path),
            "SQLite thread metadata references missing rollout JSONL `{}`; restore the artifact or remove the stale thread using supported Codex tooling before retrying",
            rollout_path.display()
        );
    }
    for rollout in rollout_files {
        validate_json_lines(
            source.home(),
            &rollout.relative_path,
            MAX_ROLLOUT_VALIDATION_BYTES,
        )
        .await?;
    }
    Ok(())
}

async fn referenced_rollout_paths(source: &SqliteConfig) -> anyhow::Result<Vec<PathBuf>> {
    let path = source.state_db_path();
    let pool = source.open_immutable_pool(&path).await.with_context(|| {
        format!(
            "failed to open SQLite state DB at {} while validating rollout references",
            path.display()
        )
    })?;
    let result = sqlx::query_scalar::<_, String>("SELECT rollout_path FROM threads ORDER BY id")
        .fetch_all(&pool)
        .await
        .context("read rollout references from SQLite state DB");
    pool.close().await;
    Ok(result?.into_iter().map(PathBuf::from).collect())
}

pub(super) fn relative_rollout_path(
    source_home: &Path,
    rollout_path: &Path,
) -> anyhow::Result<PathBuf> {
    let relative_path = if rollout_path.is_absolute() {
        rollout_path.strip_prefix(source_home).with_context(|| {
            format!(
                "SQLite thread metadata references rollout outside the source home: {}",
                rollout_path.display()
            )
        })?
    } else {
        anyhow::ensure!(
            rollout_path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
            "SQLite thread metadata references rollout outside the source home: {}",
            rollout_path.display()
        );
        rollout_path
    };
    logical_rollout_path(relative_path).ok_or_else(|| {
        anyhow::anyhow!(
            "SQLite thread metadata references non-JSONL rollout `{}`",
            rollout_path.display()
        )
    })
}

pub(super) fn logical_rollout_path(path: &Path) -> Option<PathBuf> {
    if path
        .extension()
        .is_some_and(|extension| extension == "jsonl")
    {
        return Some(path.to_path_buf());
    }
    (path.extension().is_some_and(|extension| extension == "zst")
        && path.file_stem().is_some_and(|stem| {
            Path::new(stem)
                .extension()
                .is_some_and(|ext| ext == "jsonl")
        }))
    .then(|| path.with_extension(""))
}

pub(super) async fn validate_json_lines(
    source_home: &Path,
    relative_path: &Path,
    maximum_bytes: u64,
) -> anyhow::Result<()> {
    let path = source_home.join(relative_path);
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)
            .with_context(|| format!("open rollout JSONL {}", path.display()))?;
        let mut reader: Box<dyn BufRead> =
            if path.extension().is_some_and(|extension| extension == "zst") {
                Box::new(BufReader::new(zstd::stream::read::Decoder::new(file)?))
            } else {
                Box::new(BufReader::new(file))
            };
        let mut remaining_bytes = maximum_bytes;
        for line_number in 1_usize.. {
            let mut line = Vec::new();
            let bytes = BufRead::read_until(
                &mut Read::take(&mut *reader, remaining_bytes.saturating_add(1)),
                b'\n',
                &mut line,
            )
            .with_context(|| format!("read rollout JSONL {}", path.display()))?;
            let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
            anyhow::ensure!(
                bytes <= remaining_bytes,
                "rollout JSONL {} exceeds the {maximum_bytes}-byte decoded validation budget",
                path.display()
            );
            if bytes == 0 {
                break;
            }
            remaining_bytes -= bytes;
            serde_json::from_slice::<serde_json::Value>(&line).with_context(|| {
                format!(
                    "rollout JSONL {} contains invalid JSON at line {line_number}; restore a valid artifact before retrying",
                    path.display()
                )
            })?;
        }
        anyhow::Ok(())
    })
    .await
    .context("join rollout validation task")?
}
