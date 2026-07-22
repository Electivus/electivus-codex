use super::ExternalAgentConfigImportDetailsRecord;
use super::ExternalAgentConfigImportFailureRecord;
use super::ExternalAgentConfigImportHistoryRecord;
use super::ExternalAgentConfigImportSuccessRecord;
use crate::model::datetime_to_epoch_millis;
use crate::postgres::qualified_table;
use chrono::Utc;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use sqlx::Row;

#[derive(Clone)]
pub(super) struct PostgresExternalAgentConfigImportStore {
    pool: PgPool,
    table: String,
}

impl PostgresExternalAgentConfigImportStore {
    pub(super) fn new(pool: PgPool, schema: String) -> Self {
        Self {
            pool,
            table: qualified_table(&schema, "external_agent_config_imports"),
        }
    }

    pub(super) async fn record_completed(
        &self,
        import_id: &str,
        successes: &[ExternalAgentConfigImportSuccessRecord],
        failures: &[ExternalAgentConfigImportFailureRecord],
    ) -> anyhow::Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (import_id, completed_at_ms, successes, failures) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT(import_id) DO UPDATE SET \
             completed_at_ms = excluded.completed_at_ms, successes = excluded.successes, \
             failures = excluded.failures",
            self.table
        )))
        .bind(import_id)
        .bind(datetime_to_epoch_millis(Utc::now()))
        .bind(serde_json::to_value(successes)?)
        .bind(serde_json::to_value(failures)?)
        .execute(&self.pool)
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
            "SELECT import_id, completed_at_ms, successes, failures FROM {} \
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
                    completed_at_ms: row.try_get("completed_at_ms")?,
                    successes: serde_json::from_value(successes)?,
                    failures: serde_json::from_value(failures)?,
                })
            })
            .collect()
    }
}
