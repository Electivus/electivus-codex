use super::SourceFileInventory;
use crate::SqliteConfig;
use crate::state_db_path;
use anyhow::Context;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;

pub(super) async fn validate_rollout_files(
    source: &SqliteConfig,
    rollout_files: &[SourceFileInventory],
) -> anyhow::Result<()> {
    let inventoried_paths = rollout_files
        .iter()
        .map(|file| file.relative_path.clone())
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
        validate_json_lines(source.home(), &rollout.relative_path).await?;
    }
    Ok(())
}

async fn referenced_rollout_paths(source: &SqliteConfig) -> anyhow::Result<Vec<PathBuf>> {
    let path = state_db_path(source.home());
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

fn relative_rollout_path(source_home: &Path, rollout_path: &Path) -> anyhow::Result<PathBuf> {
    let relative_path = if rollout_path.is_absolute() {
        rollout_path.strip_prefix(source_home).with_context(|| {
            format!(
                "SQLite thread metadata references rollout outside the source home: {}",
                rollout_path.display()
            )
        })?
    } else {
        rollout_path
    };
    anyhow::ensure!(
        relative_path
            .extension()
            .is_some_and(|extension| extension == "jsonl"),
        "SQLite thread metadata references non-JSONL rollout `{}`",
        rollout_path.display()
    );
    Ok(relative_path.to_path_buf())
}

async fn validate_json_lines(source_home: &Path, relative_path: &Path) -> anyhow::Result<()> {
    let path = source_home.join(relative_path);
    let file = tokio::fs::File::open(&path)
        .await
        .with_context(|| format!("open rollout JSONL {}", path.display()))?;
    let mut lines = tokio::io::BufReader::new(file).lines();
    let mut line_number = 0_u64;
    while let Some(line) = lines
        .next_line()
        .await
        .with_context(|| format!("read rollout JSONL {}", path.display()))?
    {
        line_number += 1;
        serde_json::from_str::<serde_json::Value>(&line).with_context(|| {
            format!(
                "rollout JSONL {} contains invalid JSON at line {line_number}; restore a valid artifact before retrying",
                path.display()
            )
        })?;
    }
    Ok(())
}
