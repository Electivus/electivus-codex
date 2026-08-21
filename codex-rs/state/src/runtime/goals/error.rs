use std::error::Error;
use std::fmt;

/// Result returned by the public thread-goal persistence boundary.
pub type GoalStoreResult<T> = Result<T, GoalStoreError>;

/// Stable category for a failure returned by [`super::GoalStore`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GoalStoreErrorKind {
    Conflict,
    InvalidRequest,
    Persistence,
}

/// Thread-goal operation that failed at the Runtime State boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GoalStoreOperation {
    AccountThreadGoalUsage,
    ClearThreadGoalContinuationDeferral,
    DeleteThreadGoal,
    GetThreadGoal,
    HasThreadGoalContinuationDeferral,
    InsertThreadGoal,
    PauseActiveThreadGoal,
    ReplaceThreadGoal,
    ReplaceThreadGoalSnapshot,
    UpdateThreadGoal,
    UsageLimitActiveThreadGoal,
}

impl GoalStoreOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AccountThreadGoalUsage => "account thread goal usage",
            Self::ClearThreadGoalContinuationDeferral => "clear thread goal continuation deferral",
            Self::DeleteThreadGoal => "delete thread goal",
            Self::GetThreadGoal => "get thread goal",
            Self::HasThreadGoalContinuationDeferral => "check thread goal continuation deferral",
            Self::InsertThreadGoal => "insert thread goal",
            Self::PauseActiveThreadGoal => "pause active thread goal",
            Self::ReplaceThreadGoal => "replace thread goal",
            Self::ReplaceThreadGoalSnapshot => "replace thread goal snapshot",
            Self::UpdateThreadGoal => "update thread goal",
            Self::UsageLimitActiveThreadGoal => "usage limit active thread goal",
        }
    }
}

/// Backend-independent error returned by thread-goal reads and mutations.
pub struct GoalStoreError {
    kind: GoalStoreErrorKind,
    operation: GoalStoreOperation,
    source: anyhow::Error,
}

impl GoalStoreError {
    pub fn kind(&self) -> GoalStoreErrorKind {
        self.kind
    }

    pub fn operation(&self) -> GoalStoreOperation {
        self.operation
    }

    fn from_source(operation: GoalStoreOperation, source: anyhow::Error) -> Self {
        let kind = match source.downcast_ref::<GoalStoreFailure>() {
            Some(GoalStoreFailure::AccountingEventConflict) => GoalStoreErrorKind::Conflict,
            Some(GoalStoreFailure::AccountingEventIdRequired) => GoalStoreErrorKind::InvalidRequest,
            None => GoalStoreErrorKind::Persistence,
        };
        Self {
            kind,
            operation,
            source,
        }
    }
}

impl fmt::Debug for GoalStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoalStoreError")
            .field("kind", &self.kind)
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for GoalStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let operation = self.operation.as_str();
        match self.kind {
            GoalStoreErrorKind::Conflict => write!(
                formatter,
                "Runtime State could not complete the `{operation}` operation because the accounting event conflicts with persisted goal usage"
            ),
            GoalStoreErrorKind::InvalidRequest => write!(
                formatter,
                "Runtime State could not complete the `{operation}` operation because the request is invalid"
            ),
            GoalStoreErrorKind::Persistence => write!(
                formatter,
                "Runtime State could not complete the `{operation}` operation; verify goal persistence health, then retry"
            ),
        }
    }
}

impl Error for GoalStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug)]
pub(super) enum GoalStoreFailure {
    AccountingEventConflict,
    AccountingEventIdRequired,
}

impl fmt::Display for GoalStoreFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccountingEventConflict => {
                formatter.write_str("goal accounting event was reused with different usage")
            }
            Self::AccountingEventIdRequired => {
                formatter.write_str("goal accounting event id must not be empty")
            }
        }
    }
}

impl Error for GoalStoreFailure {}

pub(super) fn public_goal_store_result<T>(
    operation: GoalStoreOperation,
    result: anyhow::Result<T>,
) -> GoalStoreResult<T> {
    result.map_err(|source| GoalStoreError::from_source(operation, source))
}
