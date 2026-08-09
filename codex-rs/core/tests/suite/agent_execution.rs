use anyhow::Result;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_thread_store::AppendThreadItemsParams;
use codex_thread_store::ArchiveThreadParams;
use codex_thread_store::ChildRegistrationGuard;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::DeleteThreadParams;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::ListThreadsParams;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ReadThreadByRolloutPathParams;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ResumeThreadParams;
use codex_thread_store::StoredModelContext;
use codex_thread_store::StoredThread;
use codex_thread_store::StoredThreadHistory;
use codex_thread_store::ThreadPage;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreFuture;
use codex_thread_store::UpdateThreadMetadataParams;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::Notify;

const FIRST_PROMPT: &str = "spawn the first worker";
const FIRST_TASK: &str = "first worker task";
const SECOND_TASK: &str = "second worker task";
const MULTI_AGENT_V2_NAMESPACE: &str = "collaboration";

struct DelayedChildCreateStore {
    inner: Arc<InMemoryThreadStore>,
    child_created: Notify,
    created_child_id: Mutex<Option<ThreadId>>,
    release_child_create: Notify,
}

impl DelayedChildCreateStore {
    fn new() -> Self {
        Self {
            inner: InMemoryThreadStore::for_id(format!(
                "delayed-child-create-{}",
                uuid::Uuid::new_v4()
            )),
            child_created: Notify::new(),
            created_child_id: Mutex::new(None),
            release_child_create: Notify::new(),
        }
    }
}

impl ThreadStore for DelayedChildCreateStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()> {
        let inner = Arc::clone(&self.inner);
        let child_created = &self.child_created;
        let created_child_id = &self.created_child_id;
        let release_child_create = &self.release_child_create;
        Box::pin(async move {
            let child_thread_id = params.parent_thread_id.map(|_| params.thread_id);
            inner.create_thread(params).await?;
            if let Some(child_thread_id) = child_thread_id {
                *created_child_id.lock().await = Some(child_thread_id);
                child_created.notify_one();
                release_child_create.notified().await;
            }
            Ok(())
        })
    }

    fn resume_thread(&self, params: ResumeThreadParams) -> ThreadStoreFuture<'_, ()> {
        self.inner.resume_thread(params)
    }

    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        self.inner.append_items(params)
    }

    fn persist_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        self.inner.persist_thread(thread_id)
    }

    fn flush_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        self.inner.flush_thread(thread_id)
    }

    fn shutdown_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        self.inner.shutdown_thread(thread_id)
    }

    fn discard_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        self.inner.discard_thread(thread_id)
    }

    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory> {
        self.inner.load_history(params)
    }

    fn load_latest_model_context(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredModelContext> {
        self.inner.load_latest_model_context(params)
    }

    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        self.inner.read_thread(params)
    }

    fn validate_child_registration(
        &self,
        thread_id: ThreadId,
    ) -> ThreadStoreFuture<'_, ChildRegistrationGuard> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            inner
                .read_thread(ReadThreadParams {
                    thread_id,
                    include_archived: true,
                    include_history: false,
                })
                .await?;
            inner.validate_child_registration(thread_id).await
        })
    }

    fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        self.inner.read_thread_by_rollout_path(params)
    }

    fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage> {
        self.inner.list_threads(params)
    }

    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        self.inner.update_thread_metadata(params)
    }

    fn archive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()> {
        self.inner.archive_thread(params)
    }

    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        self.inner.unarchive_thread(params)
    }

    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()> {
        self.inner.delete_thread(params)
    }
}

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(&request.body)
        .is_ok_and(|body| body.to_string().contains(text))
}

fn has_function_call_output(request: &wiremock::Request, call_id: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(&request.body).is_ok_and(|body| {
        body.get("input")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type").and_then(serde_json::Value::as_str)
                        == Some("function_call_output")
                        && item.get("call_id").and_then(serde_json::Value::as_str) == Some(call_id)
                })
            })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_persisted_child_is_reported_and_never_published() -> Result<()> {
    let server = start_mock_server().await;
    let call_id = "deleted-child-call";
    let args = serde_json::to_string(&json!({
        "message": "child that will be deleted",
        "task_name": "deleted",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, FIRST_PROMPT),
        sse(vec![
            ev_response_created("deleted-child-response"),
            ev_function_call_with_namespace(
                call_id,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &args,
            ),
            ev_completed("deleted-child-response"),
        ]),
    )
    .await;
    let followup = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| has_function_call_output(request, call_id),
        sse(vec![
            ev_response_created("deleted-child-followup"),
            ev_assistant_message("deleted-child-message", "spawn rejected"),
            ev_completed("deleted-child-followup"),
        ]),
    )
    .await;

    let store = Arc::new(DelayedChildCreateStore::new());
    let mut builder = test_codex()
        .with_model("koffing")
        .with_thread_store(Arc::clone(&store))
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
        });
    let test = builder.build_with_auto_env(&server).await?;
    let delete_child = async {
        store.child_created.notified().await;
        let child_thread_id = store
            .created_child_id
            .lock()
            .await
            .expect("delayed store should expose the child id");
        let deletion = store
            .inner
            .delete_thread(DeleteThreadParams {
                thread_id: child_thread_id,
            })
            .await;
        store.release_child_create.notify_one();
        deletion?;
        Ok::<_, codex_thread_store::ThreadStoreError>(child_thread_id)
    };
    let (submit_result, child_thread_id) =
        tokio::join!(test.submit_turn(FIRST_PROMPT), delete_child);
    submit_result?;
    let child_thread_id = child_thread_id?;

    assert_eq!(
        followup.function_call_output_text(call_id),
        Some(format!(
            "collab spawn failed: no thread with id: {child_thread_id}"
        ))
    );
    assert_eq!(
        test.thread_manager.list_thread_ids().await,
        vec![test.session_configured.thread_id]
    );
    assert!(
        test.thread_manager
            .get_thread(child_thread_id)
            .await
            .is_err()
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_nested_spawn_checks_shared_active_execution_capacity() -> Result<()> {
    let server = start_mock_server().await;
    let first_args = serde_json::to_string(&json!({
        "message": FIRST_TASK,
        "task_name": "first",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, FIRST_PROMPT),
        sse(vec![
            ev_response_created("first-response"),
            ev_function_call_with_namespace(
                "first-call",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &first_args,
            ),
            ev_completed("first-response"),
        ]),
    )
    .await;
    let second_args = serde_json::to_string(&json!({
        "message": SECOND_TASK,
        "task_name": "second",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, FIRST_TASK) && !has_function_call_output(request, "first-call")
        },
        sse(vec![
            ev_response_created("first-worker-response"),
            ev_function_call_with_namespace(
                "second-call",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &second_args,
            ),
            ev_completed("first-worker-response"),
        ]),
    )
    .await;
    let second_followup = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "second-call"),
        sse(vec![
            ev_response_created("second-followup-response"),
            ev_assistant_message("second-followup-message", "blocked"),
            ev_completed("second-followup-response"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "first-call"),
        sse(vec![
            ev_response_created("first-followup-response"),
            ev_assistant_message("first-followup-message", "spawned"),
            ev_completed("first-followup-response"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_model("gpt-5.6-sol")
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
            config.multi_agent_v2.max_concurrent_threads_per_session = 2;
        });
    let test = builder.build(&server).await?;
    test.submit_turn(FIRST_PROMPT).await?;

    let second_output = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(output) = second_followup.function_call_output_text("second-call") {
                return output;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(
        second_output,
        "collab spawn failed: agent thread limit reached"
    );
    assert_eq!(test.thread_manager.list_thread_ids().await.len(), 2);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_v2_agent_restore_replays_persisted_model_context() -> Result<()> {
    const SPAWN_PROMPT: &str = "spawn a V2 agent that will be unloaded";
    const RESTORE_PROMPT: &str = "reload the persisted V2 agent";
    const FIRST_CHILD_REPLY: &str = "first child persisted reply";
    const FOLLOWUP: &str = "continue after the cold restore";

    let server = start_mock_server().await;
    let first_args = serde_json::to_string(&json!({
        "message": FIRST_TASK,
        "task_name": "first",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, SPAWN_PROMPT),
        sse(vec![
            ev_response_created("cold-parent-spawn-first"),
            ev_function_call_with_namespace(
                "cold-spawn-first-call",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &first_args,
            ),
            ev_completed("cold-parent-spawn-first"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, FIRST_TASK)
                && !has_function_call_output(request, "cold-spawn-first-call")
        },
        sse(vec![
            ev_response_created("cold-first-child"),
            ev_assistant_message("cold-first-child-message", FIRST_CHILD_REPLY),
            ev_completed("cold-first-child"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "cold-spawn-first-call"),
        sse(vec![
            ev_response_created("cold-parent-spawn-complete"),
            ev_assistant_message("cold-parent-spawn-complete-message", "spawned"),
            ev_completed("cold-parent-spawn-complete"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_model("koffing")
        .with_history_mode(ThreadHistoryMode::Paginated)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
            config.multi_agent_v2.max_concurrent_threads_per_session = 3;
        });
    let test = builder.build_with_auto_env(&server).await?;
    test.submit_turn(SPAWN_PROMPT).await?;

    let child_thread_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(thread_id) = test
                .thread_manager
                .list_thread_ids()
                .await
                .into_iter()
                .find(|thread_id| *thread_id != test.session_configured.thread_id)
            {
                return thread_id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let child = test.thread_manager.get_thread(child_thread_id).await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                child.agent_status().await,
                AgentStatus::Completed(Some(ref message)) if message == FIRST_CHILD_REPLY
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    child.shutdown_and_wait().await?;
    assert!(
        test.thread_manager
            .remove_thread(&child_thread_id)
            .await
            .is_some()
    );

    let followup_args = serde_json::to_string(&json!({
        "target": "first",
        "message": FOLLOWUP,
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, RESTORE_PROMPT),
        sse(vec![
            ev_response_created("cold-parent-followup-first"),
            ev_function_call_with_namespace(
                "cold-followup-first-call",
                MULTI_AGENT_V2_NAMESPACE,
                "followup_task",
                &followup_args,
            ),
            ev_completed("cold-parent-followup-first"),
        ]),
    )
    .await;
    let restored_child = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, FIRST_TASK)
                && body_contains(request, FIRST_CHILD_REPLY)
                && body_contains(request, FOLLOWUP)
                && !has_function_call_output(request, "cold-followup-first-call")
        },
        sse(vec![
            ev_response_created("cold-restored-first-child"),
            ev_assistant_message("cold-restored-first-child-message", "restored"),
            ev_completed("cold-restored-first-child"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "cold-followup-first-call"),
        sse(vec![
            ev_response_created("cold-parent-done"),
            ev_assistant_message("cold-parent-done-message", "done"),
            ev_completed("cold-parent-done"),
        ]),
    )
    .await;
    test.submit_turn(RESTORE_PROMPT).await?;

    let restored_request = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(request) = restored_child.requests().into_iter().find(|request| {
                let body = request.body_json().to_string();
                [FIRST_TASK, FIRST_CHILD_REPLY, FOLLOWUP]
                    .iter()
                    .all(|expected| body.contains(expected))
                    && request
                        .function_call_output_text("cold-followup-first-call")
                        .is_none()
            }) {
                return request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let restored_body = restored_request.body_json().to_string();
    for expected in [FIRST_TASK, FIRST_CHILD_REPLY, FOLLOWUP] {
        assert!(
            restored_body.contains(expected),
            "restored child request should contain {expected:?}"
        );
    }

    Ok(())
}
