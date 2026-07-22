use super::GoalStore;
use super::GoalStoreBackend;
use super::GoalStoreOperation;
use super::GoalStoreResult;
use super::error::public_goal_store_result;
use crate::ThreadGoal;
use crate::model::datetime_to_epoch_millis;
use codex_protocol::ThreadId;

impl GoalStore {
    pub async fn replace_thread_goal_snapshot(&self, goal: &ThreadGoal) -> GoalStoreResult<()> {
        public_goal_store_result(
            GoalStoreOperation::ReplaceThreadGoalSnapshot,
            self.replace_thread_goal_snapshot_inner(goal).await,
        )
    }

    async fn replace_thread_goal_snapshot_inner(&self, goal: &ThreadGoal) -> anyhow::Result<()> {
        if let GoalStoreBackend::Postgres(store) = &self.backend {
            return store.replace_thread_goal_snapshot(goal).await;
        }
        let mut transaction = self.sqlite_pool()?.begin().await?;
        sqlx::query(
            r#"
INSERT INTO thread_goals (
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(thread_id) DO UPDATE SET
    goal_id = excluded.goal_id,
    objective = excluded.objective,
    status = excluded.status,
    token_budget = excluded.token_budget,
    tokens_used = excluded.tokens_used,
    time_used_seconds = excluded.time_used_seconds,
    created_at_ms = excluded.created_at_ms,
    updated_at_ms = excluded.updated_at_ms
            "#,
        )
        .bind(goal.thread_id.to_string())
        .bind(&goal.goal_id)
        .bind(&goal.objective)
        .bind(goal.status.as_str())
        .bind(goal.token_budget)
        .bind(goal.tokens_used)
        .bind(goal.time_used_seconds)
        .bind(datetime_to_epoch_millis(goal.created_at))
        .bind(datetime_to_epoch_millis(goal.updated_at))
        .execute(&mut *transaction)
        .await?;

        sqlx::query(
            r#"
INSERT INTO thread_goal_continuation_deferrals (thread_id)
VALUES (?)
ON CONFLICT(thread_id) DO NOTHING
            "#,
        )
        .bind(goal.thread_id.to_string())
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;
        Ok(())
    }

    pub async fn has_thread_goal_continuation_deferral(
        &self,
        thread_id: ThreadId,
    ) -> GoalStoreResult<bool> {
        public_goal_store_result(
            GoalStoreOperation::HasThreadGoalContinuationDeferral,
            self.has_thread_goal_continuation_deferral_inner(thread_id)
                .await,
        )
    }

    async fn has_thread_goal_continuation_deferral_inner(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<bool> {
        if let GoalStoreBackend::Postgres(store) = &self.backend {
            return store.has_thread_goal_continuation_deferral(thread_id).await;
        }
        sqlx::query_scalar(
            r#"
SELECT EXISTS(
    SELECT 1
    FROM thread_goal_continuation_deferrals
    WHERE thread_id = ?
)
            "#,
        )
        .bind(thread_id.to_string())
        .fetch_one(self.sqlite_pool()?.as_ref())
        .await
        .map_err(Into::into)
    }

    pub async fn clear_thread_goal_continuation_deferral(
        &self,
        thread_id: ThreadId,
    ) -> GoalStoreResult<()> {
        public_goal_store_result(
            GoalStoreOperation::ClearThreadGoalContinuationDeferral,
            self.clear_thread_goal_continuation_deferral_inner(thread_id)
                .await,
        )
    }

    async fn clear_thread_goal_continuation_deferral_inner(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<()> {
        if let GoalStoreBackend::Postgres(store) = &self.backend {
            return store
                .clear_thread_goal_continuation_deferral(thread_id)
                .await;
        }
        sqlx::query("DELETE FROM thread_goal_continuation_deferrals WHERE thread_id = ?")
            .bind(thread_id.to_string())
            .execute(self.sqlite_pool()?.as_ref())
            .await?;

        Ok(())
    }
}
