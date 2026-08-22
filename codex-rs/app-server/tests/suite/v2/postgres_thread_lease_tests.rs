#![allow(
    clippy::disallowed_methods,
    reason = "PostgreSQL tests connect only to PostgreSQL pools"
)]

use std::sync::Arc;

use anyhow::Result;
use app_test_support::create_command_execution_sse_response;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use codex_app_server::in_process::InProcessServerEvent;
use codex_app_server_protocol as api;
use codex_protocol::ThreadId;
use codex_thread_store as store;
use codex_thread_store::ThreadStore;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;
use tempfile::TempDir;
use tokio::time::timeout;

use super::postgres_thread_store::PostgresFixture;
use super::postgres_thread_store::request;
use super::remote_thread_store::start_in_process_server_with_thread_store;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
#[ignore = "requires CODEX_TEST_POSTGRES_URL pointing to PostgreSQL 18"]
async fn postgres_contract_active_session_recovers_expired_lease_before_interrupt() -> Result<()> {
    let fixture = PostgresFixture::new()?;
    fixture.migrate().await?;
    let codex_home = TempDir::new()?;
    let working_directory = TempDir::new()?;
    #[cfg(target_os = "windows")]
    let command = vec![
        "powershell".to_string(),
        "-Command".to_string(),
        "Start-Sleep -Seconds 10".to_string(),
    ];
    #[cfg(not(target_os = "windows"))]
    let command = vec!["sleep".to_string(), "10".to_string()];
    let model_server = create_mock_responses_server_sequence_unchecked(vec![
        create_command_execution_sse_response(
            command,
            Some(working_directory.path()),
            Some(10_000),
            "call_sleep",
        )?,
    ])
    .await;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            r#"
model = "mock-model"
model_provider = "mock_provider"
approval_policy = "never"
sandbox_mode = "danger-full-access"

[model_providers.mock_provider]
name = "Mock provider"
base_url = "{}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0

[features]
plugins = false
"#,
            model_server.uri()
        ),
    )?;

    let runtime = fixture.runtime(codex_home.path()).await?;
    let postgres_store = Arc::new(store::PostgresThreadStore::from_runtime(Arc::clone(
        &runtime,
    ))?);
    let mut client = start_in_process_server_with_thread_store(
        codex_home.path(),
        Arc::clone(&postgres_store) as Arc<dyn store::ThreadStore>,
    )
    .await?;
    let started: api::ThreadStartResponse = request(
        &client,
        api::ClientRequest::ThreadStart {
            request_id: api::RequestId::Integer(1),
            params: api::ThreadStartParams {
                history_mode: Some(api::ThreadHistoryMode::Paginated),
                ..Default::default()
            },
        },
    )
    .await?;
    let pool = sqlx::PgPool::connect(&fixture.database_url).await?;
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE \"{}\".threads SET writer_lease_expires_at = NOW() - INTERVAL '1 second' \
         WHERE thread_id = $1",
        fixture.schema
    )))
    .bind(&started.thread.id)
    .execute(&pool)
    .await?;
    pool.close().await;

    let turn: api::TurnStartResponse = request(
        &client,
        api::ClientRequest::TurnStart {
            request_id: api::RequestId::Integer(2),
            params: api::TurnStartParams {
                thread_id: started.thread.id.clone(),
                input: vec![api::UserInput::Text {
                    text: "run sleep".to_string(),
                    text_elements: Vec::new(),
                }],
                cwd: Some(working_directory.path().to_path_buf()),
                ..Default::default()
            },
        },
    )
    .await?;

    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let Some(event) = client.next_event().await else {
                anyhow::bail!("app-server stopped before the command started");
            };
            if let InProcessServerEvent::ServerNotification(notification) = event {
                match *notification {
                    api::ServerNotification::Warning(warning)
                        if warning.message.contains("writer no longer owns") =>
                    {
                        anyhow::bail!(warning.message);
                    }
                    api::ServerNotification::ItemStarted(item)
                        if matches!(item.item, api::ThreadItem::CommandExecution { .. }) =>
                    {
                        return Ok::<(), anyhow::Error>(());
                    }
                    _ => {}
                }
            }
        }
    })
    .await??;

    let _: api::TurnInterruptResponse = request(
        &client,
        api::ClientRequest::TurnInterrupt {
            request_id: api::RequestId::Integer(3),
            params: api::TurnInterruptParams {
                thread_id: started.thread.id.clone(),
                turn_id: turn.turn.id,
            },
        },
    )
    .await?;
    let completed = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let Some(event) = client.next_event().await else {
                anyhow::bail!("app-server stopped before turn/completed");
            };
            if let InProcessServerEvent::ServerNotification(notification) = event {
                match *notification {
                    api::ServerNotification::Warning(warning)
                        if warning.message.contains("writer no longer owns") =>
                    {
                        anyhow::bail!(warning.message);
                    }
                    api::ServerNotification::TurnCompleted(completed)
                        if completed.thread_id == started.thread.id =>
                    {
                        return Ok::<_, anyhow::Error>(completed);
                    }
                    _ => {}
                }
            }
        }
    })
    .await??;
    assert_eq!(completed.turn.status, api::TurnStatus::Interrupted);

    postgres_store
        .flush_thread(ThreadId::from_string(&started.thread.id)?)
        .await?;
    let turns: api::ThreadTurnsListResponse = request(
        &client,
        api::ClientRequest::ThreadTurnsList {
            request_id: api::RequestId::Integer(4),
            params: api::ThreadTurnsListParams {
                thread_id: started.thread.id.clone(),
                cursor: None,
                limit: Some(10),
                sort_direction: Some(api::SortDirection::Asc),
                items_view: Some(api::TurnItemsView::Full),
            },
        },
    )
    .await?;
    assert_eq!(turns.data.len(), 1);
    assert_eq!(turns.data[0].status, api::TurnStatus::Interrupted);

    client.shutdown().await?;
    runtime.close().await;
    fixture.cleanup().await
}
