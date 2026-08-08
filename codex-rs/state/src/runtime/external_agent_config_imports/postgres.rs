use super::ExternalAgentConfigImportDetailsRecord;
use super::ExternalAgentConfigImportFailureRecord;
use super::ExternalAgentConfigImportHistoryRecord;
use super::ExternalAgentConfigImportSuccessRecord;
use super::ExternalAgentMemoryImport;
use crate::model::datetime_to_epoch_millis;
use crate::postgres::qualified_table;
use crate::runtime::memory_store::postgres::PostgresMemoryStore;
use chrono::Utc;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use sqlx::Row;
use sqlx::Transaction;

#[derive(Clone)]
pub(super) struct PostgresExternalAgentConfigImportStore {
    memory_store: PostgresMemoryStore,
    pool: PgPool,
    table: String,
}

impl PostgresExternalAgentConfigImportStore {
    pub(super) fn new(pool: PgPool, schema: String) -> Self {
        Self {
            memory_store: PostgresMemoryStore::new(pool.clone(), schema.clone()),
            pool,
            table: qualified_table(&schema, "external_agent_config_imports"),
        }
    }

    pub(super) async fn record_completed(
        &self,
        import_id: &str,
        provider_id: Option<&str>,
        successes: &[ExternalAgentConfigImportSuccessRecord],
        failures: &[ExternalAgentConfigImportFailureRecord],
    ) -> anyhow::Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (import_id, provider_id, completed_at_ms, successes, failures) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT(import_id) DO UPDATE SET \
             provider_id = excluded.provider_id, completed_at_ms = excluded.completed_at_ms, \
             successes = excluded.successes, \
             failures = excluded.failures",
            self.table
        )))
        .bind(import_id)
        .bind(provider_id)
        .bind(datetime_to_epoch_millis(Utc::now()))
        .bind(serde_json::to_value(successes)?)
        .bind(serde_json::to_value(failures)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(super) async fn record_completed_with_memory_import(
        &self,
        import_id: &str,
        provider_id: Option<&str>,
        successes: &[ExternalAgentConfigImportSuccessRecord],
        failures: &[ExternalAgentConfigImportFailureRecord],
        memory_import: &ExternalAgentMemoryImport,
    ) -> anyhow::Result<()> {
        let fingerprint = memory_import_fingerprint(memory_import)?;
        let mut transaction = self.pool.begin().await?;
        self.memory_store
            .acquire_output_and_global_job_lock(&mut transaction)
            .await?;
        let existing_fingerprint: Option<Option<Vec<u8>>> =
            sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT memory_import_fingerprint FROM {} WHERE import_id = $1 FOR UPDATE",
                self.table
            )))
            .bind(import_id)
            .fetch_optional(&mut *transaction)
            .await?;

        if let Some(Some(existing_fingerprint)) = existing_fingerprint {
            anyhow::ensure!(
                existing_fingerprint == fingerprint,
                "external-agent import identifier was reused with different Memory Artifacts"
            );
            self.update_completed_payload_in_transaction(
                &mut transaction,
                import_id,
                provider_id,
                successes,
                failures,
            )
            .await?;
            transaction.commit().await?;
            return Ok(());
        }

        let generation_id = self
            .memory_store
            .apply_external_agent_import_in_transaction(&mut transaction, memory_import)
            .await?;
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (import_id, provider_id, completed_at_ms, successes, failures, \
             memory_import_fingerprint, memory_generation_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT(import_id) DO UPDATE SET \
             provider_id = excluded.provider_id, completed_at_ms = excluded.completed_at_ms, \
             successes = excluded.successes, \
             failures = excluded.failures, \
             memory_import_fingerprint = excluded.memory_import_fingerprint, \
             memory_generation_id = excluded.memory_generation_id",
            self.table
        )))
        .bind(import_id)
        .bind(provider_id)
        .bind(datetime_to_epoch_millis(Utc::now()))
        .bind(serde_json::to_value(successes)?)
        .bind(serde_json::to_value(failures)?)
        .bind(fingerprint)
        .bind(generation_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn update_completed_payload_in_transaction(
        &self,
        transaction: &mut Transaction<'_, sqlx::Postgres>,
        import_id: &str,
        provider_id: Option<&str>,
        successes: &[ExternalAgentConfigImportSuccessRecord],
        failures: &[ExternalAgentConfigImportFailureRecord],
    ) -> anyhow::Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET provider_id = $1, completed_at_ms = $2, successes = $3, failures = $4 \
             WHERE import_id = $5",
            self.table
        )))
        .bind(provider_id)
        .bind(datetime_to_epoch_millis(Utc::now()))
        .bind(serde_json::to_value(successes)?)
        .bind(serde_json::to_value(failures)?)
        .bind(import_id)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    pub(super) async fn details(
        &self,
        import_id: &str,
    ) -> anyhow::Result<Option<ExternalAgentConfigImportDetailsRecord>> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT successes, failures FROM {} WHERE import_id = $1",
            self.table
        )))
        .bind(import_id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            let successes: Value = row.try_get("successes")?;
            let failures: Value = row.try_get("failures")?;
            Ok(ExternalAgentConfigImportDetailsRecord {
                successes: serde_json::from_value(successes)?,
                failures: serde_json::from_value(failures)?,
            })
        })
        .transpose()
    }

    pub(super) async fn history(
        &self,
    ) -> anyhow::Result<Vec<ExternalAgentConfigImportHistoryRecord>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT import_id, provider_id, completed_at_ms, successes, failures FROM {} \
             ORDER BY completed_at_ms DESC, import_id ASC",
            self.table
        )))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let successes: Value = row.try_get("successes")?;
                let failures: Value = row.try_get("failures")?;
                Ok(ExternalAgentConfigImportHistoryRecord {
                    import_id: row.try_get("import_id")?,
                    provider_id: row.try_get("provider_id")?,
                    completed_at_ms: row.try_get("completed_at_ms")?,
                    successes: serde_json::from_value(successes)?,
                    failures: serde_json::from_value(failures)?,
                })
            })
            .collect()
    }
}

fn memory_import_fingerprint(memory_import: &ExternalAgentMemoryImport) -> anyhow::Result<Vec<u8>> {
    let mut digest = Sha256::new();
    digest.update(b"codex-external-agent-memory-import-v1\0");
    for project_key in memory_import.project_keys() {
        hash_bytes(&mut digest, project_key.as_bytes())?;
    }
    digest.update([0xff]);
    for artifact in memory_import.artifacts().artifacts() {
        hash_bytes(&mut digest, artifact.path().as_bytes())?;
        hash_bytes(&mut digest, artifact.contents())?;
    }
    Ok(digest.finalize().to_vec())
}

fn hash_bytes(digest: &mut Sha256, value: &[u8]) -> anyhow::Result<()> {
    digest.update(u64::try_from(value.len())?.to_le_bytes());
    digest.update(value);
    Ok(())
}
