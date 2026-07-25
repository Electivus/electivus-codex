use super::BackfillClaimOutcome;
use super::BackfillLease;
use crate::BackfillState;
use crate::BackfillStatus;
use crate::postgres::qualified_table;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use sqlx::Row;

#[derive(Clone)]
pub(super) struct PostgresBackfillCoordinator {
    pool: PgPool,
    table: String,
}

impl PostgresBackfillCoordinator {
    pub(super) fn new(pool: PgPool, schema: String) -> Self {
        Self {
            pool,
            table: qualified_table(&schema, "backfill_state"),
        }
    }

    pub(super) async fn state(&self) -> anyhow::Result<BackfillState> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT status, last_watermark, last_success_at FROM {} WHERE id = 1",
            self.table
        )))
        .fetch_one(&self.pool)
        .await?;
        state_from_row(&row)
    }

    pub(super) async fn try_claim(
        &self,
        owner_id: &str,
        lease_millis: i64,
    ) -> anyhow::Result<BackfillClaimOutcome> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT status, last_watermark, last_success_at, \
             COALESCE(lease_expires_at <= clock_timestamp(), TRUE) AS lease_expired \
             FROM {} WHERE id = 1 FOR UPDATE",
            self.table
        )))
        .fetch_one(transaction.as_mut())
        .await?;
        let state = state_from_row(&row)?;
        let outcome = match state.status {
            BackfillStatus::Complete => BackfillClaimOutcome::Complete(state),
            BackfillStatus::Running if !row.try_get::<bool, _>("lease_expired")? => {
                BackfillClaimOutcome::Busy(state)
            }
            BackfillStatus::Pending | BackfillStatus::Running => {
                let row = sqlx::query(AssertSqlSafe(format!(
                    "UPDATE {} SET status = 'running', owner_id = $1, \
                     fencing_token = fencing_token + 1, \
                     lease_expires_at = clock_timestamp() + $2 * INTERVAL '1 millisecond', \
                     updated_at = clock_timestamp() WHERE id = 1 \
                     RETURNING status, last_watermark, last_success_at, fencing_token",
                    self.table
                )))
                .bind(owner_id)
                .bind(lease_millis)
                .fetch_one(transaction.as_mut())
                .await?;
                BackfillClaimOutcome::Claimed {
                    lease: BackfillLease {
                        owner_id: owner_id.to_string(),
                        fencing_token: row.try_get("fencing_token")?,
                    },
                    state: state_from_row(&row)?,
                }
            }
        };
        transaction.commit().await?;
        Ok(outcome)
    }

    pub(super) async fn update_lease(
        &self,
        lease: &BackfillLease,
        lease_millis: i64,
        watermark: Option<&str>,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET last_watermark = COALESCE($1, last_watermark), \
             lease_expires_at = clock_timestamp() + $2 * INTERVAL '1 millisecond', \
             updated_at = clock_timestamp() WHERE id = 1 AND status = 'running' \
             AND owner_id = $3 AND fencing_token = $4 \
             AND lease_expires_at > clock_timestamp()",
            self.table
        )))
        .bind(watermark)
        .bind(lease_millis)
        .bind(&lease.owner_id)
        .bind(lease.fencing_token)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub(super) async fn finish_lease(
        &self,
        lease: &BackfillLease,
        status: BackfillStatus,
        last_watermark: Option<&str>,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET status = $1, last_watermark = COALESCE($2, last_watermark), \
             last_success_at = CASE WHEN $1 = 'complete' THEN clock_timestamp() \
             ELSE last_success_at END, owner_id = NULL, lease_expires_at = NULL, \
             updated_at = clock_timestamp() WHERE id = 1 AND status = 'running' \
             AND owner_id = $3 AND fencing_token = $4 \
             AND lease_expires_at > clock_timestamp()",
            self.table
        )))
        .bind(status.as_str())
        .bind(last_watermark)
        .bind(&lease.owner_id)
        .bind(lease.fencing_token)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }
}

fn state_from_row(row: &sqlx::postgres::PgRow) -> anyhow::Result<BackfillState> {
    let status: String = row.try_get("status")?;
    Ok(BackfillState {
        status: BackfillStatus::parse(&status)?,
        last_watermark: row.try_get("last_watermark")?,
        last_success_at: row.try_get("last_success_at")?,
    })
}
