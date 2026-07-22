use super::PostgresMemoryStore;
use crate::Stage1Output;
use crate::runtime::memory_store::MemoryArtifact;
use crate::runtime::memory_store::MemoryArtifactSet;
use crate::runtime::memory_store::MemoryGeneration;
use sqlx::AssertSqlSafe;
use sqlx::Row;
use uuid::Uuid;

impl PostgresMemoryStore {
    pub(crate) async fn publish_memory_generation(
        &self,
        ownership_token: &str,
        completed_watermark: i64,
        selected_outputs: &[Stage1Output],
        artifacts: &MemoryArtifactSet,
    ) -> anyhow::Result<bool> {
        let artifact_count = i32::try_from(artifacts.artifacts().len())?;
        let total_bytes = artifacts
            .artifacts()
            .iter()
            .try_fold(0_i64, |total, artifact| {
                let artifact_bytes = i64::try_from(artifact.contents().len())?;
                total.checked_add(artifact_bytes).ok_or_else(|| {
                    anyhow::anyhow!("Memory Generation size exceeds supported range")
                })
            })?;
        let generation_id = Uuid::new_v4();
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
        .execute(&mut *transaction)
        .await?;

        for artifact in artifacts.artifacts() {
            sqlx::query(AssertSqlSafe(format!(
                "INSERT INTO {} (generation_id, artifact_path, contents) VALUES ($1, $2, $3)",
                self.artifacts_table
            )))
            .bind(generation_id)
            .bind(artifact.path())
            .bind(artifact.contents())
            .execute(&mut *transaction)
            .await?;
        }

        let activated = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET active_generation_id = $1 WHERE singleton",
            self.generation_state_table
        )))
        .bind(generation_id)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        anyhow::ensure!(
            activated == 1,
            "Memory Generation active pointer is missing"
        );
        transaction.commit().await?;
        Ok(true)
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
        let Some(first) = rows.first() else {
            return Ok(None);
        };

        let mut artifacts = Vec::new();
        for row in &rows {
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
        let actual_bytes = artifacts
            .artifacts()
            .iter()
            .try_fold(0_i64, |total, artifact| {
                total
                    .checked_add(i64::try_from(artifact.contents().len())?)
                    .ok_or_else(|| {
                        anyhow::anyhow!("active Memory Generation size exceeds supported range")
                    })
            })?;
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
}
