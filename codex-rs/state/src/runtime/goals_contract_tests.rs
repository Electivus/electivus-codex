use super::GoalAccountingMode;
use super::GoalAccountingOutcome;
use super::GoalAccountingRequest;
use super::GoalAccountingTarget;
use super::GoalStore;
use super::GoalStoreError;
use super::GoalStoreErrorKind;
use super::GoalStoreOperation;
use super::GoalUpdate;
use super::StateRuntime;
use super::test_support::test_thread_metadata;
use super::test_support::unique_temp_dir;
use crate::ThreadGoal;
use crate::ThreadGoalStatus;
use anyhow::Result;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

pub(crate) async fn provoke_goal_accounting_conflict(
    store: &GoalStore,
    thread_id: ThreadId,
) -> Result<GoalStoreError> {
    let goal = store
        .replace_thread_goal(
            thread_id,
            "compare public goal errors",
            ThreadGoalStatus::Active,
            /*token_budget*/ Some(1_000),
        )
        .await?;
    let invalid_request = store
        .account_thread_goal_usage(
            thread_id,
            GoalAccountingRequest {
                event_id: " ",
                time_delta_seconds: 1,
                token_delta: 10,
                mode: GoalAccountingMode::ActiveOnly,
                target: GoalAccountingTarget::GoalId(goal.goal_id.as_str()),
            },
        )
        .await
        .expect_err("a blank accounting event id should be rejected");
    assert_eq!(invalid_request.kind(), GoalStoreErrorKind::InvalidRequest);
    assert_eq!(
        invalid_request.operation(),
        GoalStoreOperation::AccountThreadGoalUsage
    );
    let request = GoalAccountingRequest {
        event_id: "public-error-contract",
        time_delta_seconds: 3,
        token_delta: 30,
        mode: GoalAccountingMode::ActiveOnly,
        target: GoalAccountingTarget::GoalId(goal.goal_id.as_str()),
    };
    store.account_thread_goal_usage(thread_id, request).await?;

    Ok(store
        .account_thread_goal_usage(
            thread_id,
            GoalAccountingRequest {
                token_delta: 31,
                ..request
            },
        )
        .await
        .expect_err("reusing an accounting event with different usage should conflict"))
}

pub(crate) async fn collect_closed_goal_store_errors(
    store: &GoalStore,
    goal: &ThreadGoal,
) -> Vec<GoalStoreError> {
    let thread_id = goal.thread_id;
    store.close().await;

    vec![
        store
            .get_thread_goal(thread_id)
            .await
            .expect_err("get must fail after the goal store closes"),
        store
            .replace_thread_goal(
                thread_id,
                "replace after close",
                ThreadGoalStatus::Active,
                /*token_budget*/ None,
            )
            .await
            .expect_err("replace must fail after the goal store closes"),
        store
            .insert_thread_goal(
                thread_id,
                "insert after close",
                ThreadGoalStatus::Active,
                /*token_budget*/ None,
            )
            .await
            .expect_err("insert must fail after the goal store closes"),
        store
            .update_thread_goal(
                thread_id,
                GoalUpdate {
                    objective: Some("update after close".to_string()),
                    status: None,
                    token_budget: None,
                    expected_goal_id: Some(goal.goal_id.clone()),
                },
            )
            .await
            .expect_err("update must fail after the goal store closes"),
        store
            .pause_active_thread_goal(thread_id)
            .await
            .expect_err("pause must fail after the goal store closes"),
        store
            .usage_limit_active_thread_goal(thread_id)
            .await
            .expect_err("usage limit must fail after the goal store closes"),
        store
            .delete_thread_goal(thread_id)
            .await
            .expect_err("delete must fail after the goal store closes"),
        store
            .replace_thread_goal_snapshot(goal)
            .await
            .expect_err("snapshot replacement must fail after the goal store closes"),
        store
            .has_thread_goal_continuation_deferral(thread_id)
            .await
            .expect_err("deferral read must fail after the goal store closes"),
        store
            .clear_thread_goal_continuation_deferral(thread_id)
            .await
            .expect_err("deferral clear must fail after the goal store closes"),
        store
            .account_thread_goal_usage(
                thread_id,
                GoalAccountingRequest {
                    event_id: "account-after-close",
                    time_delta_seconds: 1,
                    token_delta: 10,
                    mode: GoalAccountingMode::ActiveOnly,
                    target: GoalAccountingTarget::GoalId(goal.goal_id.as_str()),
                },
            )
            .await
            .expect_err("accounting must fail after the goal store closes"),
    ]
}

pub(crate) fn goal_store_error_signature(
    error: &GoalStoreError,
) -> (GoalStoreErrorKind, GoalStoreOperation, String) {
    (error.kind(), error.operation(), error.to_string())
}

struct AccountingModeContract {
    name: &'static str,
    mode: GoalAccountingMode,
    accepted_statuses: &'static [ThreadGoalStatus],
}

const GOAL_STATUSES: [ThreadGoalStatus; 6] = [
    ThreadGoalStatus::Active,
    ThreadGoalStatus::Paused,
    ThreadGoalStatus::Blocked,
    ThreadGoalStatus::UsageLimited,
    ThreadGoalStatus::BudgetLimited,
    ThreadGoalStatus::Complete,
];

const ACCOUNTING_MODE_CONTRACTS: [AccountingModeContract; 4] = [
    AccountingModeContract {
        name: "active-status-only",
        mode: GoalAccountingMode::ActiveStatusOnly,
        accepted_statuses: &[ThreadGoalStatus::Active],
    },
    AccountingModeContract {
        name: "active-only",
        mode: GoalAccountingMode::ActiveOnly,
        accepted_statuses: &[ThreadGoalStatus::Active, ThreadGoalStatus::BudgetLimited],
    },
    AccountingModeContract {
        name: "active-or-complete",
        mode: GoalAccountingMode::ActiveOrComplete,
        accepted_statuses: &[
            ThreadGoalStatus::Active,
            ThreadGoalStatus::BudgetLimited,
            ThreadGoalStatus::Complete,
        ],
    },
    AccountingModeContract {
        name: "active-or-stopped",
        mode: GoalAccountingMode::ActiveOrStopped,
        accepted_statuses: &[
            ThreadGoalStatus::Active,
            ThreadGoalStatus::Paused,
            ThreadGoalStatus::Blocked,
            ThreadGoalStatus::UsageLimited,
            ThreadGoalStatus::BudgetLimited,
        ],
    },
];

async fn assert_accounting_status_matrix(
    writer: &GoalStore,
    reader: &GoalStore,
    thread_id: ThreadId,
) -> Result<()> {
    for contract in ACCOUNTING_MODE_CONTRACTS {
        for status in GOAL_STATUSES {
            let objective = format!("{} {status:?}", contract.name);
            let goal = writer
                .replace_thread_goal(
                    thread_id,
                    &objective,
                    status,
                    /*token_budget*/ Some(10),
                )
                .await?;
            let outcome = reader
                .account_thread_goal_usage(
                    thread_id,
                    GoalAccountingRequest {
                        event_id: &objective,
                        time_delta_seconds: 1,
                        token_delta: 11,
                        mode: contract.mode,
                        target: GoalAccountingTarget::GoalId(goal.goal_id.as_str()),
                    },
                )
                .await?;

            if contract.accepted_statuses.contains(&status) {
                let GoalAccountingOutcome::Updated(accounted) = outcome else {
                    anyhow::bail!(
                        "{} should account a {status:?} goal, got {outcome:?}",
                        contract.name
                    );
                };
                let expected = ThreadGoal {
                    status: if status == ThreadGoalStatus::Complete {
                        ThreadGoalStatus::Complete
                    } else {
                        ThreadGoalStatus::BudgetLimited
                    },
                    tokens_used: 11,
                    time_used_seconds: 1,
                    updated_at: accounted.updated_at,
                    ..goal
                };
                assert_eq!(accounted, expected);
                assert_eq!(writer.get_thread_goal(thread_id).await?, Some(expected));
            } else {
                assert_eq!(
                    outcome,
                    GoalAccountingOutcome::Unchanged(Some(goal.clone()))
                );
                assert_eq!(writer.get_thread_goal(thread_id).await?, Some(goal));
            }
        }
    }
    Ok(())
}

pub(crate) async fn run_goal_lifecycle_contract(
    writer: &GoalStore,
    reader: &GoalStore,
    thread_id: ThreadId,
) -> Result<()> {
    let replacement = reader
        .replace_thread_goal(
            thread_id,
            "verify replica visibility",
            ThreadGoalStatus::Paused,
            /*token_budget*/ None,
        )
        .await?;
    assert_eq!(
        writer.get_thread_goal(thread_id).await?,
        Some(replacement.clone())
    );

    assert_eq!(
        writer
            .insert_thread_goal(
                thread_id,
                "must not replace a stopped goal",
                ThreadGoalStatus::Active,
                /*token_budget*/ Some(20_000),
            )
            .await?,
        None
    );
    let resumed = writer
        .update_thread_goal(
            thread_id,
            GoalUpdate {
                objective: Some("resume through another replica".to_string()),
                status: Some(ThreadGoalStatus::Active),
                token_budget: Some(Some(20_000)),
                expected_goal_id: Some(replacement.goal_id.clone()),
            },
        )
        .await?
        .expect("current goal should update");
    assert_eq!(resumed.status, ThreadGoalStatus::Active);
    let blocked = reader
        .update_thread_goal(
            thread_id,
            GoalUpdate {
                objective: None,
                status: Some(ThreadGoalStatus::Blocked),
                token_budget: None,
                expected_goal_id: Some(replacement.goal_id.clone()),
            },
        )
        .await?
        .expect("active goal should become blocked");
    assert_eq!(writer.get_thread_goal(thread_id).await?, Some(blocked));
    let reactivated = writer
        .update_thread_goal(
            thread_id,
            GoalUpdate {
                objective: None,
                status: Some(ThreadGoalStatus::Active),
                token_budget: None,
                expected_goal_id: Some(replacement.goal_id.clone()),
            },
        )
        .await?
        .expect("blocked goal should resume");
    assert_eq!(reader.get_thread_goal(thread_id).await?, Some(reactivated));
    assert_eq!(
        reader
            .pause_active_thread_goal(thread_id)
            .await?
            .unwrap()
            .status,
        ThreadGoalStatus::Paused
    );
    let completed = writer
        .update_thread_goal(
            thread_id,
            GoalUpdate {
                objective: None,
                status: Some(ThreadGoalStatus::Complete),
                token_budget: None,
                expected_goal_id: Some(replacement.goal_id.clone()),
            },
        )
        .await?
        .expect("current goal should complete");
    assert_eq!(completed.status, ThreadGoalStatus::Complete);
    let inserted = reader
        .insert_thread_goal(
            thread_id,
            "replace only the completed goal",
            ThreadGoalStatus::Active,
            /*token_budget*/ Some(0),
        )
        .await?
        .expect("completed goal should be replaceable");
    assert_eq!(inserted.status, ThreadGoalStatus::BudgetLimited);
    assert_eq!(
        writer
            .update_thread_goal(
                thread_id,
                GoalUpdate {
                    objective: Some("stale update must not apply".to_string()),
                    status: Some(ThreadGoalStatus::Complete),
                    token_budget: Some(None),
                    expected_goal_id: Some(replacement.goal_id),
                },
            )
            .await?,
        None
    );
    assert_eq!(
        writer.get_thread_goal(thread_id).await?,
        Some(inserted.clone())
    );
    let preserved = writer
        .update_thread_goal(
            thread_id,
            GoalUpdate {
                objective: None,
                status: Some(ThreadGoalStatus::Paused),
                token_budget: None,
                expected_goal_id: Some(inserted.goal_id.clone()),
            },
        )
        .await?
        .expect("budget-limited goal should remain addressable");
    assert_eq!(preserved.status, ThreadGoalStatus::BudgetLimited);
    let preserved = reader
        .update_thread_goal(
            thread_id,
            GoalUpdate {
                objective: None,
                status: Some(ThreadGoalStatus::Blocked),
                token_budget: None,
                expected_goal_id: Some(inserted.goal_id.clone()),
            },
        )
        .await?
        .expect("budget-limited goal should remain addressable");
    assert_eq!(preserved.status, ThreadGoalStatus::BudgetLimited);
    assert_eq!(writer.get_thread_goal(thread_id).await?, Some(preserved));
    let usage_limited = reader
        .usage_limit_active_thread_goal(thread_id)
        .await?
        .expect("usage limit supersedes a budget limit");
    assert_eq!(usage_limited.status, ThreadGoalStatus::UsageLimited);
    assert_eq!(
        writer.delete_thread_goal(thread_id).await?,
        Some(usage_limited)
    );
    assert_eq!(reader.get_thread_goal(thread_id).await?, None);

    let deferred_goal = writer
        .replace_thread_goal(
            thread_id,
            "continue after an explicit turn",
            ThreadGoalStatus::Active,
            /*token_budget*/ Some(100),
        )
        .await?;
    reader.replace_thread_goal_snapshot(&deferred_goal).await?;
    assert!(
        writer
            .has_thread_goal_continuation_deferral(thread_id)
            .await?
    );
    reader
        .clear_thread_goal_continuation_deferral(thread_id)
        .await?;
    assert!(
        !writer
            .has_thread_goal_continuation_deferral(thread_id)
            .await?
    );

    let first_accounting = writer.account_thread_goal_usage(
        thread_id,
        GoalAccountingRequest {
            event_id: "distinct-accounting-1",
            time_delta_seconds: 4,
            token_delta: 40,
            mode: GoalAccountingMode::ActiveOnly,
            target: GoalAccountingTarget::GoalId(deferred_goal.goal_id.as_str()),
        },
    );
    let second_accounting = reader.account_thread_goal_usage(
        thread_id,
        GoalAccountingRequest {
            event_id: "distinct-accounting-2",
            time_delta_seconds: 6,
            token_delta: 60,
            mode: GoalAccountingMode::ActiveOnly,
            target: GoalAccountingTarget::GoalId(deferred_goal.goal_id.as_str()),
        },
    );
    let (first_accounting, second_accounting) = tokio::join!(first_accounting, second_accounting);
    assert!(matches!(
        first_accounting?,
        GoalAccountingOutcome::Updated(_)
    ));
    assert!(matches!(
        second_accounting?,
        GoalAccountingOutcome::Updated(_)
    ));
    let accounted = writer
        .get_thread_goal(thread_id)
        .await?
        .expect("accounted goal should exist");
    assert_eq!(accounted.tokens_used, 100);
    assert_eq!(accounted.time_used_seconds, 10);
    assert_eq!(accounted.status, ThreadGoalStatus::BudgetLimited);
    assert_eq!(reader.get_thread_goal(thread_id).await?, Some(accounted));

    let idempotent_goal = writer
        .replace_thread_goal(
            thread_id,
            "charge a retried event once",
            ThreadGoalStatus::Active,
            /*token_budget*/ Some(1_000),
        )
        .await?;
    let request = GoalAccountingRequest {
        event_id: "retried-accounting",
        time_delta_seconds: 7,
        token_delta: 70,
        mode: GoalAccountingMode::ActiveOnly,
        target: GoalAccountingTarget::GoalId(idempotent_goal.goal_id.as_str()),
    };
    let (first_retry, second_retry) = tokio::join!(
        writer.account_thread_goal_usage(thread_id, request),
        reader.account_thread_goal_usage(thread_id, request),
    );
    let outcomes = [first_retry?, second_retry?];
    assert!(matches!(
        (&outcomes[0], &outcomes[1]),
        (
            GoalAccountingOutcome::Updated(_),
            GoalAccountingOutcome::AlreadyAccounted(_)
        ) | (
            GoalAccountingOutcome::AlreadyAccounted(_),
            GoalAccountingOutcome::Updated(_)
        )
    ));
    let idempotently_accounted = writer
        .get_thread_goal(thread_id)
        .await?
        .expect("idempotently accounted goal should exist");
    assert_eq!(idempotently_accounted.tokens_used, 70);
    assert_eq!(idempotently_accounted.time_used_seconds, 7);
    assert_eq!(
        reader.get_thread_goal(thread_id).await?,
        Some(idempotently_accounted.clone())
    );
    reader
        .account_thread_goal_usage(
            thread_id,
            GoalAccountingRequest {
                token_delta: 71,
                ..request
            },
        )
        .await
        .expect_err("reusing an accounting event with different usage should fail");
    assert_eq!(
        writer.get_thread_goal(thread_id).await?,
        Some(idempotently_accounted)
    );

    assert_accounting_status_matrix(writer, reader, thread_id).await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_goal_lifecycle_satisfies_shared_contract() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let writer = StateRuntime::init_sqlite(codex_home.clone(), "test-provider".to_string()).await?;
    let reader = StateRuntime::init_sqlite(codex_home, "test-provider".to_string()).await?;
    let thread_id = ThreadId::new();
    writer
        .upsert_thread(&test_thread_metadata(
            writer.sqlite().home(),
            thread_id,
            writer.sqlite().home().join("workspace"),
        ))
        .await?;

    run_goal_lifecycle_contract(writer.thread_goals(), reader.thread_goals(), thread_id).await?;

    writer.close().await;
    reader.close().await;
    Ok(())
}

#[tokio::test]
async fn sqlite_goal_errors_expose_the_runtime_state_contract() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let runtime = StateRuntime::init_sqlite(codex_home, "test-provider".to_string()).await?;
    let thread_id = ThreadId::new();

    let error = provoke_goal_accounting_conflict(runtime.thread_goals(), thread_id).await?;

    assert_eq!(error.kind(), GoalStoreErrorKind::Conflict);
    assert_eq!(
        error.operation(),
        GoalStoreOperation::AccountThreadGoalUsage
    );
    assert_eq!(
        error.to_string(),
        "Runtime State could not complete the `account thread goal usage` operation because the accounting event conflicts with persisted goal usage"
    );
    let goal = runtime
        .thread_goals()
        .get_thread_goal(thread_id)
        .await?
        .expect("goal should exist before the store closes");
    let persistence_errors = collect_closed_goal_store_errors(runtime.thread_goals(), &goal).await;
    assert_eq!(
        persistence_errors
            .iter()
            .map(GoalStoreError::kind)
            .collect::<Vec<_>>(),
        vec![GoalStoreErrorKind::Persistence; 11]
    );
    assert_eq!(
        persistence_errors
            .iter()
            .map(GoalStoreError::operation)
            .collect::<Vec<_>>(),
        vec![
            GoalStoreOperation::GetThreadGoal,
            GoalStoreOperation::ReplaceThreadGoal,
            GoalStoreOperation::InsertThreadGoal,
            GoalStoreOperation::UpdateThreadGoal,
            GoalStoreOperation::PauseActiveThreadGoal,
            GoalStoreOperation::UsageLimitActiveThreadGoal,
            GoalStoreOperation::DeleteThreadGoal,
            GoalStoreOperation::ReplaceThreadGoalSnapshot,
            GoalStoreOperation::HasThreadGoalContinuationDeferral,
            GoalStoreOperation::ClearThreadGoalContinuationDeferral,
            GoalStoreOperation::AccountThreadGoalUsage,
        ]
    );
    runtime.close().await;
    Ok(())
}
