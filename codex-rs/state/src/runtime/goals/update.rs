use super::GoalStore;
use super::GoalStoreBackend;
use super::GoalStoreOperation;
use super::GoalStoreResult;
use super::error::public_goal_store_result;
use crate::ThreadGoal;
use crate::ThreadGoalStatus;
use crate::model::datetime_to_epoch_millis;
use chrono::Utc;
use codex_protocol::ThreadId;

pub struct GoalUpdate {
    pub objective: Option<String>,
    pub status: Option<ThreadGoalStatus>,
    pub token_budget: Option<Option<i64>>,
    pub expected_goal_id: Option<String>,
}

impl GoalStore {
    pub async fn update_thread_goal(
        &self,
        thread_id: ThreadId,
        update: GoalUpdate,
    ) -> GoalStoreResult<Option<ThreadGoal>> {
        public_goal_store_result(
            GoalStoreOperation::UpdateThreadGoal,
            self.update_thread_goal_inner(thread_id, update).await,
        )
    }

    async fn update_thread_goal_inner(
        &self,
        thread_id: ThreadId,
        update: GoalUpdate,
    ) -> anyhow::Result<Option<ThreadGoal>> {
        if let GoalStoreBackend::Postgres(store) = &self.backend {
            return store.update_thread_goal(thread_id, update).await;
        }
        let GoalUpdate {
            objective,
            status,
            token_budget,
            expected_goal_id,
        } = update;
        let objective = objective.as_deref();
        let expected_goal_id = expected_goal_id.as_deref();
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let result = match (status, token_budget) {
            (Some(status), Some(token_budget)) => {
                sqlx::query(
                    r#"
UPDATE thread_goals
SET
    objective = COALESCE(?, objective),
    status = CASE
        WHEN status = ? AND ? IN (?, ?) THEN status
        WHEN ? = 'active' AND ? IS NOT NULL AND tokens_used >= ? THEN ?
        ELSE ?
    END,
    token_budget = ?,
    updated_at_ms = ?
WHERE thread_id = ?
  AND (? IS NULL OR goal_id = ?)
            "#,
                )
                .bind(objective)
                .bind(ThreadGoalStatus::BudgetLimited.as_str())
                .bind(status.as_str())
                .bind(ThreadGoalStatus::Paused.as_str())
                .bind(ThreadGoalStatus::Blocked.as_str())
                .bind(status.as_str())
                .bind(token_budget)
                .bind(token_budget)
                .bind(ThreadGoalStatus::BudgetLimited.as_str())
                .bind(status.as_str())
                .bind(token_budget)
                .bind(now_ms)
                .bind(thread_id.to_string())
                .bind(expected_goal_id)
                .bind(expected_goal_id)
                .execute(self.sqlite_pool()?.as_ref())
                .await?
            }
            (Some(status), None) => {
                sqlx::query(
                    r#"
UPDATE thread_goals
SET
    objective = COALESCE(?, objective),
    status = CASE
        WHEN status = ? AND ? IN (?, ?) THEN status
        WHEN ? = 'active' AND token_budget IS NOT NULL AND tokens_used >= token_budget THEN ?
        ELSE ?
    END,
    updated_at_ms = ?
WHERE thread_id = ?
  AND (? IS NULL OR goal_id = ?)
            "#,
                )
                .bind(objective)
                .bind(ThreadGoalStatus::BudgetLimited.as_str())
                .bind(status.as_str())
                .bind(ThreadGoalStatus::Paused.as_str())
                .bind(ThreadGoalStatus::Blocked.as_str())
                .bind(status.as_str())
                .bind(ThreadGoalStatus::BudgetLimited.as_str())
                .bind(status.as_str())
                .bind(now_ms)
                .bind(thread_id.to_string())
                .bind(expected_goal_id)
                .bind(expected_goal_id)
                .execute(self.sqlite_pool()?.as_ref())
                .await?
            }
            (None, Some(token_budget)) => {
                sqlx::query(
                    r#"
UPDATE thread_goals
SET
    objective = COALESCE(?, objective),
    token_budget = ?,
    status = CASE
        WHEN status = 'active' AND ? IS NOT NULL AND tokens_used >= ? THEN ?
        ELSE status
    END,
    updated_at_ms = ?
WHERE thread_id = ?
  AND (? IS NULL OR goal_id = ?)
            "#,
                )
                .bind(objective)
                .bind(token_budget)
                .bind(token_budget)
                .bind(token_budget)
                .bind(ThreadGoalStatus::BudgetLimited.as_str())
                .bind(now_ms)
                .bind(thread_id.to_string())
                .bind(expected_goal_id)
                .bind(expected_goal_id)
                .execute(self.sqlite_pool()?.as_ref())
                .await?
            }
            (None, None) => {
                if let Some(objective) = objective {
                    sqlx::query(
                        r#"
UPDATE thread_goals
SET
    objective = ?,
    updated_at_ms = ?
WHERE thread_id = ?
  AND (? IS NULL OR goal_id = ?)
            "#,
                    )
                    .bind(objective)
                    .bind(now_ms)
                    .bind(thread_id.to_string())
                    .bind(expected_goal_id)
                    .bind(expected_goal_id)
                    .execute(self.sqlite_pool()?.as_ref())
                    .await?
                } else {
                    let goal = self.get_thread_goal_inner(thread_id).await?;
                    return Ok(match (goal, expected_goal_id) {
                        (Some(goal), Some(expected_goal_id))
                            if goal.goal_id != expected_goal_id =>
                        {
                            None
                        }
                        (goal, _) => goal,
                    });
                }
            }
        };

        if result.rows_affected() == 0 {
            return Ok(None);
        }

        self.get_thread_goal_inner(thread_id).await
    }

    pub async fn pause_active_thread_goal(
        &self,
        thread_id: ThreadId,
    ) -> GoalStoreResult<Option<ThreadGoal>> {
        public_goal_store_result(
            GoalStoreOperation::PauseActiveThreadGoal,
            self.update_active_thread_goal_status(thread_id, ThreadGoalStatus::Paused)
                .await,
        )
    }

    pub async fn usage_limit_active_thread_goal(
        &self,
        thread_id: ThreadId,
    ) -> GoalStoreResult<Option<ThreadGoal>> {
        public_goal_store_result(
            GoalStoreOperation::UsageLimitActiveThreadGoal,
            self.update_active_thread_goal_status(thread_id, ThreadGoalStatus::UsageLimited)
                .await,
        )
    }

    async fn update_active_thread_goal_status(
        &self,
        thread_id: ThreadId,
        status: ThreadGoalStatus,
    ) -> anyhow::Result<Option<ThreadGoal>> {
        if let GoalStoreBackend::Postgres(store) = &self.backend {
            return store
                .update_active_thread_goal_status(thread_id, status)
                .await;
        }
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let result = sqlx::query(
            r#"
UPDATE thread_goals
SET
    status = ?,
    updated_at_ms = ?
WHERE thread_id = ?
  AND (
      status = 'active'
      OR (
          ? = 'usage_limited'
          AND status = 'budget_limited'
      )
  )
            "#,
        )
        .bind(status.as_str())
        .bind(now_ms)
        .bind(thread_id.to_string())
        .bind(status.as_str())
        .execute(self.sqlite_pool()?.as_ref())
        .await?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }

        self.get_thread_goal_inner(thread_id).await
    }
}
