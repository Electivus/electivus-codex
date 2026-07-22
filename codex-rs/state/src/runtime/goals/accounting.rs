use super::GoalStore;
use super::GoalStoreBackend;
use super::GoalStoreOperation;
use super::GoalStoreResult;
use super::error::GoalStoreFailure;
use super::error::public_goal_store_result;
use super::thread_goal_from_row;
use crate::ThreadGoal;
use crate::ThreadGoalStatus;
use crate::model::datetime_to_epoch_millis;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::QueryBuilder;
use sqlx::Sqlite;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalAccountingOutcome {
    AlreadyAccounted(ThreadGoal),
    Unchanged(Option<ThreadGoal>),
    Updated(ThreadGoal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalAccountingMode {
    ActiveStatusOnly,
    ActiveOnly,
    ActiveOrComplete,
    ActiveOrStopped,
}

impl GoalAccountingMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::ActiveStatusOnly => "active_status_only",
            Self::ActiveOnly => "active_only",
            Self::ActiveOrComplete => "active_or_complete",
            Self::ActiveOrStopped => "active_or_stopped",
        }
    }

    pub(super) fn accepts(self, status: ThreadGoalStatus) -> bool {
        status == ThreadGoalStatus::Active
            || match self {
                Self::ActiveStatusOnly => false,
                Self::ActiveOnly => status == ThreadGoalStatus::BudgetLimited,
                Self::ActiveOrComplete => matches!(
                    status,
                    ThreadGoalStatus::BudgetLimited | ThreadGoalStatus::Complete
                ),
                Self::ActiveOrStopped => status != ThreadGoalStatus::Complete,
            }
    }

    fn applies_budget_limit(self, status: ThreadGoalStatus) -> bool {
        if status == ThreadGoalStatus::Complete {
            return false;
        }
        self == Self::ActiveOrStopped || status == ThreadGoalStatus::Active
    }
}

/// Selects which goal may receive one accounting event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalAccountingTarget<'a> {
    CurrentGoal,
    GoalId(&'a str),
}

impl GoalAccountingTarget<'_> {
    pub(super) fn matches(self, goal_id: &str) -> bool {
        match self {
            Self::CurrentGoal => true,
            Self::GoalId(expected_goal_id) => expected_goal_id == goal_id,
        }
    }
}

/// Describes one retry-safe usage increment for a thread goal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GoalAccountingRequest<'a> {
    pub event_id: &'a str,
    pub time_delta_seconds: i64,
    pub token_delta: i64,
    pub mode: GoalAccountingMode,
    pub target: GoalAccountingTarget<'a>,
}

#[derive(Debug, sqlx::FromRow)]
pub(super) struct RecordedGoalAccountingEvent {
    goal_id: String,
    time_delta_seconds: i64,
    token_delta: i64,
    mode: String,
}

impl RecordedGoalAccountingEvent {
    pub(super) fn matches(
        &self,
        request: GoalAccountingRequest<'_>,
        current_goal_id: &str,
    ) -> bool {
        self.goal_id == current_goal_id
            && request.target.matches(&self.goal_id)
            && self.time_delta_seconds == request.time_delta_seconds.max(0)
            && self.token_delta == request.token_delta.max(0)
            && self.mode == request.mode.as_str()
    }
}

impl GoalStore {
    pub async fn account_thread_goal_usage(
        &self,
        thread_id: ThreadId,
        request: GoalAccountingRequest<'_>,
    ) -> GoalStoreResult<GoalAccountingOutcome> {
        public_goal_store_result(
            GoalStoreOperation::AccountThreadGoalUsage,
            self.account_thread_goal_usage_inner(thread_id, request)
                .await,
        )
    }

    async fn account_thread_goal_usage_inner(
        &self,
        thread_id: ThreadId,
        request: GoalAccountingRequest<'_>,
    ) -> anyhow::Result<GoalAccountingOutcome> {
        if request.event_id.trim().is_empty() {
            return Err(GoalStoreFailure::AccountingEventIdRequired.into());
        }
        if let GoalStoreBackend::Postgres(store) = &self.backend {
            return store.account_thread_goal_usage(thread_id, request).await;
        }
        self.account_thread_goal_usage_sqlite(thread_id, request)
            .await
    }

    async fn account_thread_goal_usage_sqlite(
        &self,
        thread_id: ThreadId,
        request: GoalAccountingRequest<'_>,
    ) -> anyhow::Result<GoalAccountingOutcome> {
        let time_delta_seconds = request.time_delta_seconds.max(0);
        let token_delta = request.token_delta.max(0);
        if time_delta_seconds == 0 && token_delta == 0 {
            return Ok(GoalAccountingOutcome::Unchanged(
                self.get_thread_goal_inner(thread_id).await?,
            ));
        }

        let thread_id = thread_id.to_string();
        let mut transaction = self.sqlite_pool()?.begin().await?;
        sqlx::query("UPDATE thread_goals SET thread_id = thread_id WHERE thread_id = ?")
            .bind(&thread_id)
            .execute(&mut *transaction)
            .await?;
        let current_row = sqlx::query("SELECT * FROM thread_goals WHERE thread_id = ?")
            .bind(&thread_id)
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(current_row) = current_row else {
            transaction.commit().await?;
            return Ok(GoalAccountingOutcome::Unchanged(None));
        };
        let current = thread_goal_from_row(&current_row)?;

        let recorded = sqlx::query_as::<_, RecordedGoalAccountingEvent>(
            "SELECT goal_id, time_delta_seconds, token_delta, mode \
             FROM thread_goal_accounting_events WHERE thread_id = ? AND event_id = ?",
        )
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

        sqlx::query(
            "INSERT INTO thread_goal_accounting_events \
             (thread_id, event_id, goal_id, time_delta_seconds, token_delta, mode) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&thread_id)
        .bind(request.event_id)
        .bind(&current.goal_id)
        .bind(time_delta_seconds)
        .bind(token_delta)
        .bind(request.mode.as_str())
        .execute(&mut *transaction)
        .await?;

        let mode = request.mode;
        let expected_goal_id = match request.target {
            GoalAccountingTarget::CurrentGoal => None,
            GoalAccountingTarget::GoalId(goal_id) => Some(goal_id),
        };
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let active_or_stopped_status_filter =
            "status IN ('active', 'paused', 'blocked', 'usage_limited', 'budget_limited')";
        let status_filter = match mode {
            GoalAccountingMode::ActiveStatusOnly => "status = 'active'",
            GoalAccountingMode::ActiveOnly => "status IN ('active', 'budget_limited')",
            GoalAccountingMode::ActiveOrComplete => {
                "status IN ('active', 'budget_limited', 'complete')"
            }
            GoalAccountingMode::ActiveOrStopped => active_or_stopped_status_filter,
        };
        let budget_limit_status_filter = match mode {
            GoalAccountingMode::ActiveStatusOnly
            | GoalAccountingMode::ActiveOnly
            | GoalAccountingMode::ActiveOrComplete => "status = 'active'",
            GoalAccountingMode::ActiveOrStopped => active_or_stopped_status_filter,
        };
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
UPDATE thread_goals
SET
    time_used_seconds = time_used_seconds +
            "#,
        );
        builder.push_bind(time_delta_seconds);
        builder.push(
            r#",
    tokens_used = tokens_used +
            "#,
        );
        builder.push_bind(token_delta);
        builder.push(
            r#",
    status = CASE
        WHEN
            "#,
        );
        builder.push(budget_limit_status_filter);
        builder.push(
            r#"
            AND token_budget IS NOT NULL
            AND tokens_used +
            "#,
        );
        builder.push_bind(token_delta);
        builder.push(
            r#"
                >= token_budget
            THEN
            "#,
        );
        builder.push_bind(ThreadGoalStatus::BudgetLimited.as_str());
        builder.push(
            r#"
        ELSE status
    END,
    updated_at_ms =
            "#,
        );
        builder.push_bind(now_ms);
        builder.push(
            r#"
WHERE thread_id =
            "#,
        );
        builder.push_bind(thread_id.to_string());
        builder.push(" AND ");
        builder.push(status_filter);
        if let Some(expected_goal_id) = expected_goal_id {
            builder.push(" AND goal_id = ").push_bind(expected_goal_id);
        }
        builder.push(
            r#"
RETURNING
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
            "#,
        );

        let row = builder.build().fetch_optional(&mut *transaction).await?;

        let Some(row) = row else {
            transaction.rollback().await?;
            return Ok(GoalAccountingOutcome::Unchanged(Some(current)));
        };

        let updated = thread_goal_from_row(&row)?;
        transaction.commit().await?;
        Ok(GoalAccountingOutcome::Updated(updated))
    }
}

pub(super) fn status_after_accounting(
    goal: &ThreadGoal,
    token_delta: i64,
    mode: GoalAccountingMode,
) -> ThreadGoalStatus {
    if mode.applies_budget_limit(goal.status)
        && goal
            .token_budget
            .is_some_and(|budget| goal.tokens_used + token_delta >= budget)
    {
        ThreadGoalStatus::BudgetLimited
    } else {
        goal.status
    }
}
