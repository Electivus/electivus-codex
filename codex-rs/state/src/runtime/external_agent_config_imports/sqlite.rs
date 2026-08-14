use super::ExternalAgentConfigImportDetailsRecord;
use super::ExternalAgentConfigImportFailureRecord;
use super::ExternalAgentConfigImportHistoryRecord;
use super::ExternalAgentConfigImportSuccessRecord;
use crate::model::datetime_to_epoch_millis;
use chrono::Utc;
use sqlx::Row;
use sqlx::SqlitePool;
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct SqliteExternalAgentConfigImportStore {
    pool: Arc<SqlitePool>,
}

impl SqliteExternalAgentConfigImportStore {
    pub(super) fn new(pool: Arc<SqlitePool>) -> Self {
        Self { pool }
    }

    pub(super) async fn record_completed(
        &self,
        import_id: &str,
        provider_id: Option<&str>,
        successes: &[ExternalAgentConfigImportSuccessRecord],
        failures: &[ExternalAgentConfigImportFailureRecord],
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"
INSERT INTO external_agent_config_imports (
    import_id,
    provider_id,
    completed_at_ms,
    successes,
    failures
) VALUES (?, ?, ?, ?, ?)
ON CONFLICT(import_id) DO UPDATE SET
    provider_id = excluded.provider_id,
    completed_at_ms = excluded.completed_at_ms,
    successes = excluded.successes,
    failures = excluded.failures
"#,
        )
        .bind(import_id)
        .bind(provider_id)
        .bind(datetime_to_epoch_millis(Utc::now()))
        .bind(serde_json::to_string(successes)?)
        .bind(serde_json::to_string(failures)?)
        .execute(self.pool.as_ref())
        .await?;

        Ok(())
    }

    pub(super) async fn details(
        &self,
        import_id: &str,
    ) -> anyhow::Result<Option<ExternalAgentConfigImportDetailsRecord>> {
        let row = sqlx::query(
            "SELECT successes, failures FROM external_agent_config_imports WHERE import_id = ?",
        )
        .bind(import_id)
        .fetch_optional(self.pool.as_ref())
        .await?;

        row.map(|row| {
            let successes: String = row.try_get("successes")?;
            let failures: String = row.try_get("failures")?;
            Ok(ExternalAgentConfigImportDetailsRecord {
                successes: serde_json::from_str(&successes)?,
                failures: serde_json::from_str(&failures)?,
            })
        })
        .transpose()
    }

    pub(super) async fn history(
        &self,
    ) -> anyhow::Result<Vec<ExternalAgentConfigImportHistoryRecord>> {
        let rows = sqlx::query(
            "SELECT import_id, provider_id, completed_at_ms, successes, failures \
             FROM external_agent_config_imports \
             ORDER BY completed_at_ms DESC, import_id ASC",
        )
        .fetch_all(self.pool.as_ref())
        .await?;

        rows.into_iter()
            .map(|row| {
                let successes: String = row.try_get("successes")?;
                let failures: String = row.try_get("failures")?;
                Ok(ExternalAgentConfigImportHistoryRecord {
                    import_id: row.try_get("import_id")?,
                    provider_id: row.try_get("provider_id")?,
                    completed_at_ms: row.try_get("completed_at_ms")?,
                    successes: serde_json::from_str(&successes)?,
                    failures: serde_json::from_str(&failures)?,
                })
            })
            .collect()
    }
}
