use super::GoalAccountingMode;
use super::GoalAccountingOutcome;
use super::GoalStore;
use super::GoalUpdate;
use super::StateRuntime;
use super::test_support::test_thread_metadata;
use super::test_support::unique_temp_dir;
use crate::ThreadGoalStatus;
use anyhow::Result;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

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
                expected_goal_id: Some(replacement.goal_id),
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
        /*time_delta_seconds*/ 4,
        /*token_delta*/ 40,
        GoalAccountingMode::ActiveOnly,
        Some(deferred_goal.goal_id.as_str()),
    );
    let second_accounting = reader.account_thread_goal_usage(
        thread_id,
        /*time_delta_seconds*/ 6,
        /*token_delta*/ 60,
        GoalAccountingMode::ActiveOnly,
        Some(deferred_goal.goal_id.as_str()),
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
    Ok(())
}

#[tokio::test]
async fn sqlite_goal_lifecycle_satisfies_shared_contract() -> Result<()> {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let writer = StateRuntime::init(codex_home.clone(), "test-provider".to_string()).await?;
    let reader = StateRuntime::init(codex_home, "test-provider".to_string()).await?;
    let thread_id = ThreadId::new();
    writer
        .upsert_thread(&test_thread_metadata(
            writer.codex_home(),
            thread_id,
            writer.codex_home().join("workspace"),
        ))
        .await?;

    run_goal_lifecycle_contract(writer.thread_goals(), reader.thread_goals(), thread_id).await?;

    writer.close().await;
    reader.close().await;
    Ok(())
}
