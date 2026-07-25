use crate::SqliteConfig;
use anyhow::Context;
use serde_json::Value;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct OperationalSnapshot {
    pub(super) logs: Vec<OperationalLogSnapshot>,
    pub(super) goals: Vec<OperationalGoalSnapshot>,
    pub(super) accounting_events: Vec<OperationalGoalAccountingEventSnapshot>,
    pub(super) enrollments: Vec<OperationalRemoteControlEnrollmentSnapshot>,
    pub(super) imports: Vec<OperationalExternalAgentImportSnapshot>,
}

#[derive(Debug, Eq, PartialEq, sqlx::FromRow)]
pub(super) struct OperationalLogSnapshot {
    pub(super) id: i64,
    pub(super) ts: i64,
    pub(super) ts_nanos: i64,
    pub(super) level: String,
    pub(super) target: String,
    pub(super) feedback_log_body: Option<String>,
    pub(super) module_path: Option<String>,
    pub(super) file: Option<String>,
    pub(super) line: Option<i64>,
    pub(super) thread_id: Option<String>,
    pub(super) process_uuid: Option<String>,
    pub(super) estimated_bytes: i64,
}

#[derive(Debug, Eq, PartialEq, sqlx::FromRow)]
pub(super) struct OperationalGoalSnapshot {
    pub(super) thread_id: String,
    pub(super) goal_id: String,
    pub(super) objective: String,
    pub(super) status: String,
    pub(super) token_budget: Option<i64>,
    pub(super) tokens_used: i64,
    pub(super) time_used_seconds: i64,
    pub(super) created_at_ms: i64,
    pub(super) updated_at_ms: i64,
    pub(super) continuation_deferred: bool,
}

#[derive(Debug, Eq, PartialEq, sqlx::FromRow)]
pub(super) struct OperationalGoalAccountingEventSnapshot {
    pub(super) thread_id: String,
    pub(super) event_id: String,
    pub(super) goal_id: String,
    pub(super) time_delta_seconds: i64,
    pub(super) token_delta: i64,
    pub(super) mode: String,
}

#[derive(Debug, Eq, PartialEq, sqlx::FromRow)]
pub(super) struct OperationalRemoteControlEnrollmentSnapshot {
    pub(super) websocket_url: String,
    pub(super) account_id: String,
    pub(super) app_server_client_name: String,
    pub(super) server_id: String,
    pub(super) environment_id: String,
    pub(super) server_name: String,
    pub(super) remote_control_enabled: Option<bool>,
    pub(super) updated_at: i64,
}

#[derive(sqlx::FromRow)]
struct OperationalExternalAgentImportRow {
    import_id: String,
    provider_id: Option<String>,
    completed_at_ms: i64,
    successes: String,
    failures: String,
}

#[derive(Debug, Eq, PartialEq, sqlx::FromRow)]
pub(super) struct OperationalExternalAgentImportSnapshot {
    pub(super) import_id: String,
    pub(super) provider_id: Option<String>,
    pub(super) completed_at_ms: i64,
    pub(super) successes: Value,
    pub(super) failures: Value,
}

pub(super) async fn snapshot_operational_state(
    source: &SqliteConfig,
) -> anyhow::Result<OperationalSnapshot> {
    let logs_pool = source.open_immutable_pool(&source.logs_db_path()).await?;
    let logs = sqlx::query_as::<_, OperationalLogSnapshot>(
        "SELECT id, ts, ts_nanos, level, target, feedback_log_body, module_path, file, line, \
         thread_id, process_uuid, estimated_bytes FROM logs ORDER BY id",
    )
    .fetch_all(&logs_pool)
    .await;
    logs_pool.close().await;
    let logs = logs.context("read operational logs from SQLite")?;

    let goals_pool = source.open_immutable_pool(&source.goals_db_path()).await?;
    let goals_result = async {
        let goals = sqlx::query_as::<_, OperationalGoalSnapshot>(
            "SELECT goals.thread_id, goals.goal_id, goals.objective, goals.status, \
             goals.token_budget, goals.tokens_used, goals.time_used_seconds, goals.created_at_ms, \
             goals.updated_at_ms, EXISTS(SELECT 1 FROM thread_goal_continuation_deferrals deferrals \
             WHERE deferrals.thread_id = goals.thread_id) AS continuation_deferred \
             FROM thread_goals goals ORDER BY thread_id",
        )
        .fetch_all(&goals_pool)
        .await?;
        let accounting_events = sqlx::query_as::<_, OperationalGoalAccountingEventSnapshot>(
            "SELECT thread_id, event_id, goal_id, time_delta_seconds, token_delta, mode \
             FROM thread_goal_accounting_events ORDER BY thread_id, event_id",
        )
        .fetch_all(&goals_pool)
        .await?;
        anyhow::Ok((goals, accounting_events))
    }
    .await;
    goals_pool.close().await;
    let (goals, accounting_events) = goals_result.context("read operational goals from SQLite")?;

    let state_pool = source.open_immutable_pool(&source.state_db_path()).await?;
    let state_result = async {
        let enrollments = sqlx::query_as::<_, OperationalRemoteControlEnrollmentSnapshot>(
            "SELECT websocket_url, account_id, app_server_client_name, server_id, environment_id, \
             server_name, remote_control_enabled, updated_at FROM remote_control_enrollments \
             ORDER BY websocket_url, account_id, app_server_client_name",
        )
        .fetch_all(&state_pool)
        .await?;
        let imports = sqlx::query_as::<_, OperationalExternalAgentImportRow>(
            "SELECT import_id, provider_id, completed_at_ms, successes, failures \
             FROM external_agent_config_imports ORDER BY import_id",
        )
        .fetch_all(&state_pool)
        .await?;
        let imports = imports
            .into_iter()
            .map(
                |row| -> anyhow::Result<OperationalExternalAgentImportSnapshot> {
                    Ok(OperationalExternalAgentImportSnapshot {
                        import_id: row.import_id,
                        provider_id: row.provider_id,
                        completed_at_ms: row.completed_at_ms,
                        successes: serde_json::from_str(&row.successes)?,
                        failures: serde_json::from_str(&row.failures)?,
                    })
                },
            )
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::Ok((enrollments, imports))
    }
    .await;
    state_pool.close().await;
    let (enrollments, imports) = state_result.context("read operational state from SQLite")?;
    Ok(OperationalSnapshot {
        logs,
        goals,
        accounting_events,
        enrollments,
        imports,
    })
}
