use super::PostgresMemoryStore;
use crate::Stage1Output;
use crate::runtime::external_agent_config_imports::ExternalAgentMemoryImport;
use crate::runtime::external_agent_config_imports::IMPORTED_MEMORY_EXTENSION_PREFIX;
use crate::runtime::external_agent_config_imports::IMPORTED_MEMORY_RESOURCES_PREFIX;
use crate::runtime::memory_store::MemoryArtifact;
use crate::runtime::memory_store::MemoryArtifactSet;
use crate::runtime::memory_store::MemoryGeneration;
use sqlx::AssertSqlSafe;
use sqlx::Postgres;
use sqlx::Row;
use sqlx::Transaction;
use sqlx::postgres::PgRow;
use uuid::Uuid;

impl PostgresMemoryStore {
    pub(crate) async fn publish_memory_generation(
        &self,
        ownership_token: &str,
        completed_watermark: i64,
        selected_outputs: &[Stage1Output],
        artifacts: &MemoryArtifactSet,
    ) -> anyhow::Result<bool> {
        let mut transaction = self.pool.begin().await?;
        if !self
            .complete_global_phase2_job_in_transaction(
                &mut transaction,
                ownership_token,
                completed_watermark,
                selected_outputs,
            )
            .await?
        {
            transaction.commit().await?;
            return Ok(false);
        }
        let artifacts = self
            .preserve_active_external_agent_resources(&mut transaction, artifacts)
            .await?;
        self.insert_and_activate_generation(&mut transaction, completed_watermark, &artifacts)
            .await?;
        transaction.commit().await?;
        Ok(true)
    }

    pub(crate) async fn apply_external_agent_import_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        memory_import: &ExternalAgentMemoryImport,
    ) -> anyhow::Result<Option<Uuid>> {
        let current = self
            .load_active_memory_generation_in_transaction(transaction)
            .await?;
        let current_generation_id = current
            .as_ref()
            .map(|generation| Uuid::parse_str(generation.generation_id()))
            .transpose()?;
        let completed_watermark = current
            .as_ref()
            .map(MemoryGeneration::completed_watermark)
            .unwrap_or(0);
        let mut artifacts = current
            .as_ref()
            .map(|generation| generation.artifacts().to_vec())
            .unwrap_or_default();
        artifacts.retain(|artifact| {
            !memory_import.project_keys().iter().any(|project_key| {
                artifact
                    .path()
                    .starts_with(&format!("{IMPORTED_MEMORY_RESOURCES_PREFIX}{project_key}/"))
            })
        });
        for imported_artifact in memory_import.artifacts().artifacts() {
            artifacts.retain(|artifact| artifact.path() != imported_artifact.path());
            artifacts.push(imported_artifact.clone());
        }
        let artifacts = MemoryArtifactSet::new(artifacts)?;
        if current
            .as_ref()
            .is_some_and(|generation| generation.artifacts() == artifacts.artifacts())
            || current.is_none() && artifacts.artifacts().is_empty()
        {
            return Ok(current_generation_id);
        }

        let generation_id = self
            .insert_and_activate_generation(transaction, completed_watermark, &artifacts)
            .await?;
        let now: i64 =
            sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
                .fetch_one(&mut **transaction)
                .await?;
        self.enqueue_global_consolidation_in_transaction(transaction, now)
            .await?;
        Ok(Some(generation_id))
    }

    pub(crate) async fn load_active_memory_generation(
        &self,
    ) -> anyhow::Result<Option<MemoryGeneration>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT generation.generation_id::text AS generation_id, \
             generation.completed_watermark, generation.published_at, \
             generation.artifact_count, generation.total_bytes, \
             artifact.artifact_path, artifact.contents FROM {} AS state \
             JOIN {} AS generation ON generation.generation_id = state.active_generation_id \
             LEFT JOIN {} AS artifact ON artifact.generation_id = generation.generation_id \
             WHERE state.singleton ORDER BY artifact.artifact_path",
            self.generation_state_table, self.generations_table, self.artifacts_table
        )))
        .fetch_all(&self.pool)
        .await?;
        memory_generation_from_rows(&rows)
    }

    pub(in crate::runtime::memory_store) async fn load_active_memory_generation_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
    ) -> anyhow::Result<Option<MemoryGeneration>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT generation.generation_id::text AS generation_id, \
             generation.completed_watermark, generation.published_at, \
             generation.artifact_count, generation.total_bytes, \
             artifact.artifact_path, artifact.contents FROM {} AS state \
             JOIN {} AS generation ON generation.generation_id = state.active_generation_id \
             LEFT JOIN {} AS artifact ON artifact.generation_id = generation.generation_id \
             WHERE state.singleton ORDER BY artifact.artifact_path",
            self.generation_state_table, self.generations_table, self.artifacts_table
        )))
        .fetch_all(&mut **transaction)
        .await?;
        memory_generation_from_rows(&rows)
    }

    async fn preserve_active_external_agent_resources(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        artifacts: &MemoryArtifactSet,
    ) -> anyhow::Result<MemoryArtifactSet> {
        let mut merged = artifacts
            .artifacts()
            .iter()
            .filter(|artifact| {
                !artifact
                    .path()
                    .starts_with(IMPORTED_MEMORY_EXTENSION_PREFIX)
            })
            .cloned()
            .collect::<Vec<_>>();
        if let Some(active) = self
            .load_active_memory_generation_in_transaction(transaction)
            .await?
        {
            merged.extend(
                active
                    .artifacts()
                    .iter()
                    .filter(|artifact| {
                        artifact
                            .path()
                            .starts_with(IMPORTED_MEMORY_EXTENSION_PREFIX)
                    })
                    .cloned(),
            );
        }
        MemoryArtifactSet::new(merged)
    }

    pub(in crate::runtime::memory_store) async fn insert_and_activate_generation(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        completed_watermark: i64,
        artifacts: &MemoryArtifactSet,
    ) -> anyhow::Result<Uuid> {
        let artifact_count = i32::try_from(artifacts.artifacts().len())?;
        let total_bytes = total_artifact_bytes(artifacts)?;
        let generation_id = Uuid::new_v4();
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (generation_id, completed_watermark, published_at, artifact_count, \
             total_bytes) VALUES ($1, $2, \
             FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint, $3, $4)",
            self.generations_table
        )))
        .bind(generation_id)
        .bind(completed_watermark)
        .bind(artifact_count)
        .bind(total_bytes)
        .execute(&mut **transaction)
        .await?;

        for artifact in artifacts.artifacts() {
            sqlx::query(AssertSqlSafe(format!(
                "INSERT INTO {} (generation_id, artifact_path, contents) VALUES ($1, $2, $3)",
                self.artifacts_table
            )))
            .bind(generation_id)
            .bind(artifact.path())
            .bind(artifact.contents())
            .execute(&mut **transaction)
            .await?;
        }
        let activated = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET active_generation_id = $1 WHERE singleton",
            self.generation_state_table
        )))
        .bind(generation_id)
        .execute(&mut **transaction)
        .await?
        .rows_affected();
        anyhow::ensure!(
            activated == 1,
            "Memory Generation active pointer is missing"
        );
        Ok(generation_id)
    }
}

fn memory_generation_from_rows(rows: &[PgRow]) -> anyhow::Result<Option<MemoryGeneration>> {
    let Some(first) = rows.first() else {
        return Ok(None);
    };

    let mut artifacts = Vec::new();
    for row in rows {
        if let Some(path) = row.try_get::<Option<String>, _>("artifact_path")? {
            artifacts.push(MemoryArtifact::new(path, row.try_get("contents")?)?);
        }
    }
    let artifacts = MemoryArtifactSet::new(artifacts)?;
    let expected_count: i32 = first.try_get("artifact_count")?;
    anyhow::ensure!(
        usize::try_from(expected_count)? == artifacts.artifacts().len(),
        "active Memory Generation artifact count does not match its contents"
    );
    let expected_bytes: i64 = first.try_get("total_bytes")?;
    let actual_bytes = total_artifact_bytes(&artifacts)?;
    anyhow::ensure!(
        expected_bytes == actual_bytes,
        "active Memory Generation byte count does not match its contents"
    );

    Ok(Some(MemoryGeneration::new(
        first.try_get("generation_id")?,
        first.try_get("completed_watermark")?,
        first.try_get("published_at")?,
        artifacts,
    )))
}

fn total_artifact_bytes(artifacts: &MemoryArtifactSet) -> anyhow::Result<i64> {
    artifacts
        .artifacts()
        .iter()
        .try_fold(0_i64, |total, artifact| {
            let artifact_bytes = i64::try_from(artifact.contents().len())?;
            total
                .checked_add(artifact_bytes)
                .ok_or_else(|| anyhow::anyhow!("Memory Generation size exceeds supported range"))
        })
}
