use super::GoalAccountingOutcome;
use super::GoalAccountingRequest;
use super::GoalUpdate;
use super::accounting::RecordedGoalAccountingEvent;
use super::accounting::status_after_accounting;
use super::error::GoalStoreFailure;
use super::status_after_budget_limit;
use crate::ThreadGoal;
use crate::ThreadGoalStatus;
use crate::model::datetime_to_epoch_millis;
use crate::model::epoch_millis_to_datetime;
use crate::postgres::qualified_table;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

#[derive(Clone)]
pub(super) struct PostgresGoalStore {
    accounting_events_table: String,
    pool: PgPool,
    deferrals_table: String,
    goals_table: String,
}

impl PostgresGoalStore {
    pub(super) fn new(pool: PgPool, schema: String) -> Self {
        Self {
            accounting_events_table: qualified_table(&schema, "thread_goal_accounting_events"),
            pool,
            deferrals_table: qualified_table(&schema, "thread_goal_continuation_deferrals"),
            goals_table: qualified_table(&schema, "thread_goals"),
        }
    }

    pub(super) async fn get_thread_goal(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<ThreadGoal>> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT thread_id, goal_id, objective, status, token_budget, tokens_used, \
             time_used_seconds, created_at_ms, updated_at_ms FROM {} WHERE thread_id = $1",
            self.goals_table
        )))
        .bind(thread_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| thread_goal_from_row(&row)).transpose()
    }

    pub(super) async fn replace_thread_goal_snapshot(
        &self,
        goal: &ThreadGoal,
    ) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (thread_id, goal_id, objective, status, token_budget, tokens_used, \
             time_used_seconds, created_at_ms, updated_at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT(thread_id) DO UPDATE SET \
             goal_id = excluded.goal_id, objective = excluded.objective, \
             status = excluded.status, token_budget = excluded.token_budget, \
             tokens_used = excluded.tokens_used, time_used_seconds = excluded.time_used_seconds, \
             created_at_ms = excluded.created_at_ms, updated_at_ms = excluded.updated_at_ms",
            self.goals_table
        )))
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
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (thread_id) VALUES ($1) ON CONFLICT(thread_id) DO NOTHING",
            self.deferrals_table
        )))
        .bind(goal.thread_id.to_string())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(super) async fn has_thread_goal_continuation_deferral(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<bool> {
        sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT EXISTS(SELECT 1 FROM {} WHERE thread_id = $1)",
            self.deferrals_table
        )))
        .bind(thread_id.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    pub(super) async fn clear_thread_goal_continuation_deferral(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM {} WHERE thread_id = $1",
            self.deferrals_table
        )))
        .bind(thread_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(super) async fn replace_thread_goal(
        &self,
        thread_id: ThreadId,
        objective: &str,
        status: ThreadGoalStatus,
        token_budget: Option<i64>,
    ) -> anyhow::Result<ThreadGoal> {
        let goal_id = Uuid::new_v4().to_string();
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let status = status_after_budget_limit(status, /*tokens_used*/ 0, token_budget);
        let row = sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (thread_id, goal_id, objective, status, token_budget, tokens_used, \
             time_used_seconds, created_at_ms, updated_at_ms) \
             VALUES ($1, $2, $3, $4, $5, 0, 0, $6, $7) \
             ON CONFLICT(thread_id) DO UPDATE SET \
             goal_id = excluded.goal_id, objective = excluded.objective, \
             status = excluded.status, token_budget = excluded.token_budget, tokens_used = 0, \
             time_used_seconds = 0, created_at_ms = excluded.created_at_ms, \
             updated_at_ms = excluded.updated_at_ms \
             RETURNING thread_id, goal_id, objective, status, token_budget, tokens_used, \
             time_used_seconds, created_at_ms, updated_at_ms",
            self.goals_table
        )))
        .bind(thread_id.to_string())
        .bind(goal_id)
        .bind(objective)
        .bind(status.as_str())
        .bind(token_budget)
        .bind(now_ms)
        .bind(now_ms)
        .fetch_one(&self.pool)
        .await?;

        thread_goal_from_row(&row)
    }

    pub(super) async fn insert_thread_goal(
        &self,
        thread_id: ThreadId,
        objective: &str,
        status: ThreadGoalStatus,
        token_budget: Option<i64>,
    ) -> anyhow::Result<Option<ThreadGoal>> {
        let goal_id = Uuid::new_v4().to_string();
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let status = status_after_budget_limit(status, /*tokens_used*/ 0, token_budget);
        let row = sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} AS current (thread_id, goal_id, objective, status, token_budget, \
             tokens_used, time_used_seconds, created_at_ms, updated_at_ms) \
             VALUES ($1, $2, $3, $4, $5, 0, 0, $6, $7) \
             ON CONFLICT(thread_id) DO UPDATE SET \
             goal_id = excluded.goal_id, objective = excluded.objective, \
             status = excluded.status, token_budget = excluded.token_budget, tokens_used = 0, \
             time_used_seconds = 0, created_at_ms = excluded.created_at_ms, \
             updated_at_ms = excluded.updated_at_ms WHERE current.status = 'complete' \
             RETURNING thread_id, goal_id, objective, status, token_budget, tokens_used, \
             time_used_seconds, created_at_ms, updated_at_ms",
            self.goals_table
        )))
        .bind(thread_id.to_string())
        .bind(goal_id)
        .bind(objective)
        .bind(status.as_str())
        .bind(token_budget)
        .bind(now_ms)
        .bind(now_ms)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| thread_goal_from_row(&row)).transpose()
    }

    pub(super) async fn update_thread_goal(
        &self,
        thread_id: ThreadId,
        update: GoalUpdate,
    ) -> anyhow::Result<Option<ThreadGoal>> {
        let GoalUpdate {
            objective,
            status,
            token_budget,
            expected_goal_id,
        } = update;
        if objective.is_none() && status.is_none() && token_budget.is_none() {
            let goal = self.get_thread_goal(thread_id).await?;
            return Ok(match (goal, expected_goal_id.as_deref()) {
                (Some(goal), Some(expected_goal_id)) if goal.goal_id != expected_goal_id => None,
                (goal, _) => goal,
            });
        }

        let now_ms = datetime_to_epoch_millis(Utc::now());
        let set_status = status.is_some();
        let set_token_budget = token_budget.is_some();
        let status = status.map(ThreadGoalStatus::as_str);
        let token_budget = token_budget.flatten();
        let row = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET \
             objective = COALESCE($1, objective), \
             status = CASE \
               WHEN $2 AND status = 'budget_limited' AND $4 IN ('paused', 'blocked') THEN status \
               WHEN $2 AND $4 = 'active' \
                 AND (CASE WHEN $3 THEN $5 ELSE token_budget END) IS NOT NULL \
                 AND tokens_used >= (CASE WHEN $3 THEN $5 ELSE token_budget END) \
                 THEN 'budget_limited' \
               WHEN $2 THEN $4 \
               WHEN $3 AND status = 'active' AND $5 IS NOT NULL AND tokens_used >= $5 \
                 THEN 'budget_limited' \
               ELSE status \
             END, \
             token_budget = CASE WHEN $3 THEN $5 ELSE token_budget END, \
             updated_at_ms = $6 \
             WHERE thread_id = $7 AND ($8::text IS NULL OR goal_id = $8) \
             RETURNING thread_id, goal_id, objective, status, token_budget, tokens_used, \
             time_used_seconds, created_at_ms, updated_at_ms",
            self.goals_table
        )))
        .bind(objective.as_deref())
        .bind(set_status)
        .bind(set_token_budget)
        .bind(status)
        .bind(token_budget)
        .bind(now_ms)
        .bind(thread_id.to_string())
        .bind(expected_goal_id.as_deref())
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| thread_goal_from_row(&row)).transpose()
    }

    pub(super) async fn update_active_thread_goal_status(
        &self,
        thread_id: ThreadId,
        status: ThreadGoalStatus,
    ) -> anyhow::Result<Option<ThreadGoal>> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET status = $1, updated_at_ms = $2 \
             WHERE thread_id = $3 AND (status = 'active' OR \
             ($1 = 'usage_limited' AND status = 'budget_limited')) \
             RETURNING thread_id, goal_id, objective, status, token_budget, tokens_used, \
             time_used_seconds, created_at_ms, updated_at_ms",
            self.goals_table
        )))
        .bind(status.as_str())
        .bind(datetime_to_epoch_millis(Utc::now()))
        .bind(thread_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| thread_goal_from_row(&row)).transpose()
    }

    pub(super) async fn delete_thread_goal(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<ThreadGoal>> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM {} WHERE thread_id = $1 \
             RETURNING thread_id, goal_id, objective, status, token_budget, tokens_used, \
             time_used_seconds, created_at_ms, updated_at_ms",
            self.goals_table
        )))
        .bind(thread_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| thread_goal_from_row(&row)).transpose()
    }

    pub(super) async fn account_thread_goal_usage(
        &self,
        thread_id: ThreadId,
        request: GoalAccountingRequest<'_>,
    ) -> anyhow::Result<GoalAccountingOutcome> {
        let time_delta_seconds = request.time_delta_seconds.max(0);
        let token_delta = request.token_delta.max(0);
        if time_delta_seconds == 0 && token_delta == 0 {
            return Ok(GoalAccountingOutcome::Unchanged(
                self.get_thread_goal(thread_id).await?,
            ));
        }

        let thread_id = thread_id.to_string();
        let mut transaction = self.pool.begin().await?;
        let current_row = sqlx::query(AssertSqlSafe(format!(
            "SELECT thread_id, goal_id, objective, status, token_budget, tokens_used, \
             time_used_seconds, created_at_ms, updated_at_ms FROM {} \
             WHERE thread_id = $1 FOR UPDATE",
            self.goals_table
        )))
        .bind(&thread_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(current_row) = current_row else {
            transaction.commit().await?;
            return Ok(GoalAccountingOutcome::Unchanged(None));
        };
        let current = thread_goal_from_row(&current_row)?;

        let recorded = sqlx::query_as::<_, RecordedGoalAccountingEvent>(AssertSqlSafe(format!(
            "SELECT goal_id, time_delta_seconds, token_delta, mode FROM {} \
             WHERE thread_id = $1 AND event_id = $2",
            self.accounting_events_table
        )))
        .bind(&thread_id)
        .bind(request.event_id)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(recorded) = recorded {
            if !recorded.matches(request, &current.goal_id) {
                return Err(GoalStoreFailure::AccountingEventConflict.into());
            }
            transaction.commit().await?;
            return Ok(GoalAccountingOutcome::AlreadyAccounted(current));
        }
        if !request.target.matches(&current.goal_id) || !request.mode.accepts(current.status) {
            transaction.commit().await?;
            return Ok(GoalAccountingOutcome::Unchanged(Some(current)));
        }

        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (thread_id, event_id, goal_id, time_delta_seconds, token_delta, mode) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            self.accounting_events_table
        )))
        .bind(&thread_id)
        .bind(request.event_id)
        .bind(&current.goal_id)
        .bind(time_delta_seconds)
        .bind(token_delta)
        .bind(request.mode.as_str())
        .execute(&mut *transaction)
        .await?;

        let next_status = status_after_accounting(&current, token_delta, request.mode);
        let row = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET time_used_seconds = time_used_seconds + $1, \
             tokens_used = tokens_used + $2, status = $3, updated_at_ms = $4 \
             WHERE thread_id = $5 AND goal_id = $6 \
             RETURNING thread_id, goal_id, objective, status, token_budget, tokens_used, \
             time_used_seconds, created_at_ms, updated_at_ms",
            self.goals_table
        )))
        .bind(time_delta_seconds)
        .bind(token_delta)
        .bind(next_status.as_str())
        .bind(datetime_to_epoch_millis(Utc::now()))
        .bind(&thread_id)
        .bind(&current.goal_id)
        .fetch_one(&mut *transaction)
        .await?;
        let updated = thread_goal_from_row(&row)?;
        transaction.commit().await?;
        Ok(GoalAccountingOutcome::Updated(updated))
    }

    pub(super) async fn close(&self) {
        self.pool.close().await;
    }
}

fn thread_goal_from_row(row: &PgRow) -> anyhow::Result<ThreadGoal> {
    Ok(ThreadGoal {
        thread_id: ThreadId::try_from(row.try_get::<String, _>("thread_id")?)?,
        goal_id: row.try_get("goal_id")?,
        objective: row.try_get("objective")?,
        status: ThreadGoalStatus::try_from(row.try_get::<String, _>("status")?.as_str())?,
        token_budget: row.try_get("token_budget")?,
        tokens_used: row.try_get("tokens_used")?,
        time_used_seconds: row.try_get("time_used_seconds")?,
        created_at: epoch_millis_to_datetime(row.try_get("created_at_ms")?)?,
        updated_at: epoch_millis_to_datetime(row.try_get("updated_at_ms")?)?,
    })
}
