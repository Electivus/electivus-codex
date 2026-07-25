use super::RuntimeStateMigrationInventory;
use super::SourceFileInventory;
use crate::MemoryArtifact;
use crate::MemoryArtifactSet;
use crate::SqliteConfig;
use anyhow::Context;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::path::Component;

type SqliteMemoryOutputRow = (
    String,
    i64,
    String,
    String,
    Option<String>,
    i64,
    Option<i64>,
    Option<i64>,
    bool,
    Option<i64>,
);
type SqliteMemoryJobRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

pub(super) struct MemoryMigrationSnapshot {
    pub(super) outputs: Vec<MemoryStage1OutputSnapshot>,
    pub(super) jobs: Vec<MemoryJobSnapshot>,
    pub(super) artifacts: MemoryArtifactSet,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct MemoryStage1OutputSnapshot {
    pub(super) thread_id: String,
    pub(super) source_updated_at: i64,
    pub(super) raw_memory: String,
    pub(super) rollout_summary: String,
    pub(super) rollout_slug: Option<String>,
    pub(super) generated_at: i64,
    pub(super) usage_count: Option<i64>,
    pub(super) last_usage: Option<i64>,
    pub(super) selected_for_phase2: bool,
    pub(super) selected_for_phase2_source_updated_at: Option<i64>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct MemoryJobSnapshot {
    pub(super) kind: MemoryJobKind,
    pub(super) job_key: String,
    pub(super) status: MemoryJobStatus,
    pub(super) worker_id: Option<String>,
    pub(super) ownership_token: Option<String>,
    pub(super) started_at: Option<i64>,
    pub(super) finished_at: Option<i64>,
    pub(super) lease_until: Option<i64>,
    pub(super) retry_at: Option<i64>,
    pub(super) retry_remaining: i64,
    pub(super) last_error: Option<String>,
    pub(super) input_watermark: Option<i64>,
    pub(super) last_success_watermark: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MemoryJobKind {
    MemoryStage1,
    MemoryConsolidateGlobal,
}

impl MemoryJobKind {
    pub(super) fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "memory_stage1" => Ok(Self::MemoryStage1),
            "memory_consolidate_global" => Ok(Self::MemoryConsolidateGlobal),
            _ => anyhow::bail!("memory state contains unsupported job kind `{value}`"),
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::MemoryStage1 => "memory_stage1",
            Self::MemoryConsolidateGlobal => "memory_consolidate_global",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MemoryJobStatus {
    Pending,
    Running,
    Done,
    Error,
}

impl MemoryJobStatus {
    pub(super) fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "done" => Ok(Self::Done),
            "error" => Ok(Self::Error),
            _ => anyhow::bail!("memory state contains unsupported job status `{value}`"),
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Error => "error",
        }
    }
}

impl MemoryMigrationSnapshot {
    pub(super) fn completed_watermark(&self) -> i64 {
        self.jobs
            .iter()
            .find(|job| {
                job.kind == MemoryJobKind::MemoryConsolidateGlobal && job.job_key == "global"
            })
            .and_then(|job| job.last_success_watermark)
            .unwrap_or(0)
    }

    pub(super) fn artifact_set_hash(&self) -> String {
        let mut hasher = Sha256::new();
        for artifact in self.artifacts.artifacts() {
            hash_field(&mut hasher, artifact.path().as_bytes());
            hash_field(&mut hasher, &Sha256::digest(artifact.contents()));
        }
        format!("{:x}", hasher.finalize())
    }
}

pub(super) async fn snapshot_memory_state(
    source: &SqliteConfig,
    inventory: &RuntimeStateMigrationInventory,
) -> anyhow::Result<MemoryMigrationSnapshot> {
    let pool = source
        .open_immutable_pool(&source.memories_db_path())
        .await?;
    let records = async {
        let outputs = sqlx::query_as::<_, SqliteMemoryOutputRow>(
            "SELECT thread_id, source_updated_at, raw_memory, rollout_summary, rollout_slug, \
             generated_at, usage_count, last_usage, selected_for_phase2 != 0, \
             selected_for_phase2_source_updated_at FROM stage1_outputs ORDER BY thread_id",
        )
        .fetch_all(&pool)
        .await?
        .into_iter()
        .map(output_from_row)
        .collect();
        let jobs = sqlx::query_as::<_, SqliteMemoryJobRow>(
            "SELECT kind, job_key, status, worker_id, ownership_token, started_at, finished_at, \
             lease_until, retry_at, retry_remaining, last_error, input_watermark, \
             last_success_watermark FROM jobs ORDER BY kind, job_key",
        )
        .fetch_all(&pool)
        .await?
        .into_iter()
        .map(job_from_row)
        .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::Ok((outputs, jobs))
    }
    .await;
    pool.close().await;
    let (outputs, jobs) = records.context("read memory state from SQLite")?;
    let mut files = inventory
        .memory_files
        .iter()
        .chain(&inventory.imported_resources)
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let mut artifacts = Vec::with_capacity(files.len());
    for file in files {
        if let Some(artifact) = snapshot_artifact(source, file).await? {
            artifacts.push(artifact);
        }
    }
    Ok(MemoryMigrationSnapshot {
        outputs,
        jobs,
        artifacts: MemoryArtifactSet::new(artifacts)?,
    })
}

fn output_from_row(row: SqliteMemoryOutputRow) -> MemoryStage1OutputSnapshot {
    MemoryStage1OutputSnapshot {
        thread_id: row.0,
        source_updated_at: row.1,
        raw_memory: row.2,
        rollout_summary: row.3,
        rollout_slug: row.4,
        generated_at: row.5,
        usage_count: row.6,
        last_usage: row.7,
        selected_for_phase2: row.8,
        selected_for_phase2_source_updated_at: row.9,
    }
}

fn job_from_row(row: SqliteMemoryJobRow) -> anyhow::Result<MemoryJobSnapshot> {
    Ok(MemoryJobSnapshot {
        kind: MemoryJobKind::parse(&row.0)?,
        job_key: row.1,
        status: MemoryJobStatus::parse(&row.2)?,
        worker_id: row.3,
        ownership_token: row.4,
        started_at: row.5,
        finished_at: row.6,
        lease_until: row.7,
        retry_at: row.8,
        retry_remaining: row.9,
        last_error: row.10,
        input_watermark: row.11,
        last_success_watermark: row.12,
    })
}

async fn snapshot_artifact(
    source: &SqliteConfig,
    inventory: &SourceFileInventory,
) -> anyhow::Result<Option<MemoryArtifact>> {
    let relative_path = inventory
        .relative_path
        .strip_prefix("memories")
        .context("inventoried Memory Artifact is outside the memory workspace")?;
    let components = relative_path
        .components()
        .map(|component| match component {
            Component::Normal(component) => component
                .to_str()
                .context("Memory Artifact path is not valid Unicode"),
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => {
                anyhow::bail!("Memory Artifact path must be workspace-relative")
            }
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let artifact_path = components.join("/");
    if artifact_path == "phase2_workspace_diff.md"
        || artifact_path == ".git"
        || artifact_path.starts_with(".git/")
    {
        return Ok(None);
    }
    let contents = tokio::fs::read(source.home().join(&inventory.relative_path)).await?;
    anyhow::ensure!(
        u64::try_from(contents.len())? == inventory.size_bytes
            && format!("{:x}", Sha256::digest(&contents)) == inventory.sha256,
        "Memory Artifact `{artifact_path}` changed after migration preflight"
    );
    Ok(Some(MemoryArtifact::new(artifact_path, contents)?))
}

pub(super) fn records_hash(records: &impl Serialize) -> anyhow::Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(records)?)
    ))
}

pub(super) fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}
