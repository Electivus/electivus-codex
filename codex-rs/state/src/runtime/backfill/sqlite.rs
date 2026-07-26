use super::BackfillClaimOutcome;
use super::BackfillLease;
use crate::BackfillState;
use crate::BackfillStatus;
use sqlx::Connection;
use sqlx::Row;
use sqlx::SqlitePool;
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct SqliteBackfillCoordinator {
    pool: Arc<SqlitePool>,
}

impl SqliteBackfillCoordinator {
    pub(super) fn new(pool: Arc<SqlitePool>) -> Self {
        Self { pool }
    }

    pub(super) async fn state(&self) -> anyhow::Result<BackfillState> {
        super::super::ensure_backfill_state_row_in_pool(self.pool.as_ref()).await?;
        let row = sqlx::query(
            "SELECT status, last_watermark, last_success_at FROM backfill_state WHERE id = 1",
        )
        .fetch_one(self.pool.as_ref())
        .await?;
        BackfillState::try_from_row(&row)
    }

    pub(super) async fn try_claim(
        &self,
        owner_id: &str,
        lease_millis: i64,
    ) -> anyhow::Result<BackfillClaimOutcome> {
        super::super::ensure_backfill_state_row_in_pool(self.pool.as_ref()).await?;
        let mut connection = self.pool.acquire().await?;
        let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(
            "UPDATE backfill_state SET status = ?, owner_id = ?, \
             fencing_token = fencing_token + 1, \
             lease_expires_at_ms = CAST(unixepoch('subsec') * 1000 AS INTEGER) + ?, \
             updated_at = CAST(unixepoch() AS INTEGER) WHERE id = 1 AND status != ? \
             AND (status != ? OR COALESCE(lease_expires_at_ms, 0) \
             <= CAST(unixepoch('subsec') * 1000 AS INTEGER)) \
             RETURNING status, last_watermark, last_success_at, fencing_token",
        )
        .bind(BackfillStatus::Running.as_str())
        .bind(owner_id)
        .bind(lease_millis)
        .bind(BackfillStatus::Complete.as_str())
        .bind(BackfillStatus::Running.as_str())
        .fetch_optional(transaction.as_mut())
        .await?;
        let outcome = if let Some(row) = row {
            BackfillClaimOutcome::Claimed {
                lease: BackfillLease {
                    owner_id: owner_id.to_string(),
                    fencing_token: row.try_get("fencing_token")?,
                },
                state: BackfillState::try_from_row(&row)?,
            }
        } else {
            let row = sqlx::query(
                "SELECT status, last_watermark, last_success_at FROM backfill_state WHERE id = 1",
            )
            .fetch_one(transaction.as_mut())
            .await?;
            let state = BackfillState::try_from_row(&row)?;
            match state.status {
                BackfillStatus::Complete => BackfillClaimOutcome::Complete(state),
                BackfillStatus::Pending | BackfillStatus::Running => {
                    BackfillClaimOutcome::Busy(state)
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
        let result = sqlx::query(
            "UPDATE backfill_state SET last_watermark = COALESCE(?, last_watermark), \
             lease_expires_at_ms = CAST(unixepoch('subsec') * 1000 AS INTEGER) + ?, \
             updated_at = CAST(unixepoch() AS INTEGER) \
             WHERE id = 1 AND status = ? AND owner_id = ? AND fencing_token = ? \
             AND lease_expires_at_ms > CAST(unixepoch('subsec') * 1000 AS INTEGER)",
        )
        .bind(watermark)
        .bind(lease_millis)
        .bind(BackfillStatus::Running.as_str())
        .bind(&lease.owner_id)
        .bind(lease.fencing_token)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub(super) async fn finish_lease(
        &self,
        lease: &BackfillLease,
        status: BackfillStatus,
        last_watermark: Option<&str>,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE backfill_state SET status = ?, last_watermark = COALESCE(?, last_watermark), \
             last_success_at = CASE WHEN ? = 'complete' THEN unixepoch() ELSE last_success_at END, \
             owner_id = NULL, lease_expires_at_ms = NULL, updated_at = unixepoch() \
             WHERE id = 1 AND status = 'running' AND owner_id = ? AND fencing_token = ? \
             AND lease_expires_at_ms > CAST(unixepoch('subsec') * 1000 AS INTEGER)",
        )
        .bind(status.as_str())
        .bind(last_watermark)
        .bind(status.as_str())
        .bind(&lease.owner_id)
        .bind(lease.fencing_token)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }
}
