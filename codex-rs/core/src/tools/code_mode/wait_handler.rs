use serde::Deserialize;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use crate::tools::timing::TimingParameter;
use crate::tools::timing::YieldTimingClass;
use crate::tools::timing::adjustment_message;
use crate::tools::timing::error_with_timing_adjustment;
use crate::tools::timing::resolve_yield_timing;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use super::ExecContext;
use super::WAIT_TOOL_NAME;
use super::handle_runtime_response;
use super::telemetry::CodeModeToolCallGuard;
use super::wait_spec::create_wait_tool;

pub struct CodeModeWaitHandler {
    yield_time: codex_config::ToolExecutionTimingRange,
}

impl CodeModeWaitHandler {
    pub(crate) fn new(yield_time: codex_config::ToolExecutionTimingRange) -> Self {
        Self { yield_time }
    }
}

impl Default for CodeModeWaitHandler {
    fn default() -> Self {
        Self::new(codex_config::ToolExecutionPolicy::default().yield_time())
    }
}

#[derive(Debug, Deserialize)]
struct ExecWaitArgs {
    cell_id: String,
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    terminate: bool,
}

fn parse_arguments<T>(arguments: &str) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
    })
}

impl ToolExecutor<ToolInvocation> for CodeModeWaitHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WAIT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_wait_tool(self.yield_time)
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl CodeModeWaitHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        let mut telemetry = CodeModeToolCallGuard::new(
            session.services.analytics_events_client.clone(),
            session.thread_id.to_string(),
            turn.sub_id.clone(),
            turn.turn_metadata_state.clone(),
            call_id.clone(),
            WAIT_TOOL_NAME,
        );
        let result = match payload {
            ToolPayload::Function { arguments }
                if tool_name.is_default_namespace()
                    && tool_name.name.as_str() == WAIT_TOOL_NAME =>
            {
                let args: ExecWaitArgs = parse_arguments(&arguments).inspect_err(|_error| {
                    telemetry.finish(/*success*/ false);
                })?;
                let exec = ExecContext { session, turn };
                let started_at = std::time::Instant::now();
                telemetry.cell_id = Some(args.cell_id.clone());
                let cell_id = codex_code_mode::CellId::new(args.cell_id);
                let (wait_response, resolved_yield) = if args.terminate {
                    (
                        exec.session
                            .services
                            .code_mode_service
                            .terminate(cell_id)
                            .await,
                        None,
                    )
                } else {
                    let resolved_yield = resolve_yield_timing(
                        exec.turn.config.tool_execution,
                        YieldTimingClass::Global,
                        args.yield_time_ms,
                    );
                    (
                        exec.session
                            .services
                            .code_mode_service
                            .wait(codex_code_mode::WaitRequest {
                                cell_id,
                                yield_time_ms: resolved_yield.effective_ms,
                            })
                            .await,
                        Some(resolved_yield),
                    )
                };
                let adjust_error = |error: String| match resolved_yield {
                    Some(timing) => {
                        error_with_timing_adjustment(TimingParameter::Yield, timing, error)
                    }
                    None => error,
                };
                let wait_response = wait_response.map_err(|err| {
                    telemetry.finish(/*success*/ false);
                    FunctionCallError::RespondToModel(adjust_error(err))
                })?;
                if let codex_code_mode::WaitOutcome::LiveCell(response) = &wait_response
                    && !matches!(response, codex_code_mode::RuntimeResponse::Yielded { .. })
                {
                    // Only a live-cell wait can close a CodeCell. A missing
                    // cell is still an ordinary `wait` tool result, but there
                    // is no runtime object for the reducer to complete.
                    let runtime_cell_id = match response {
                        codex_code_mode::RuntimeResponse::Yielded { cell_id, .. }
                        | codex_code_mode::RuntimeResponse::Terminated { cell_id, .. }
                        | codex_code_mode::RuntimeResponse::Result { cell_id, .. } => cell_id,
                    };
                    telemetry.cell_id = Some(runtime_cell_id.to_string());
                    if let Some(executed_tool_calls) =
                        exec.session.services.executed_tool_calls.as_ref()
                    {
                        executed_tool_calls.register_cell(runtime_cell_id, &call_id);
                    }
                    exec.session
                        .services
                        .rollout_thread_trace
                        .code_cell_trace_context(
                            exec.turn.sub_id.as_str(),
                            runtime_cell_id.as_str(),
                        )
                        .record_ended(response);
                    exec.session
                        .services
                        .code_mode_service
                        .finish_cell_dispatch(runtime_cell_id);
                    exec.session
                        .services
                        .analytics_events_client
                        .track_code_mode_tool_call(
                            codex_analytics::CodeModeToolCallFact::CellClosed {
                                thread_id: exec.session.thread_id.to_string(),
                                turn_id: exec.turn.sub_id.clone(),
                                cell_id: runtime_cell_id.to_string(),
                            },
                        );
                }
                if let Some(code_mode_host_duration) = wait_response.code_mode_host_duration() {
                    telemetry.record_code_mode_host_duration(code_mode_host_duration);
                }
                exec.session.services.elicitations.wait_until_clear().await;
                let wall_time = wait_response
                    .code_mode_host_duration()
                    .unwrap_or_else(|| started_at.elapsed());
                let mut output = handle_runtime_response(
                    &exec,
                    wait_response.into(),
                    args.max_tokens,
                    wall_time,
                )
                .await
                .map_err(|err| {
                    telemetry.finish(/*success*/ false);
                    FunctionCallError::RespondToModel(adjust_error(err))
                })?;
                if let Some(message) = resolved_yield
                    .and_then(|timing| adjustment_message(TimingParameter::Yield, timing))
                {
                    output.prepend_text(message);
                }
                Ok(boxed_tool_output(output))
            }
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{WAIT_TOOL_NAME} expects JSON arguments"
            ))),
        };
        telemetry.finish(
            result
                .as_ref()
                .is_ok_and(codex_tools::ToolOutput::success_for_logging),
        );
        result
    }
}

impl CoreToolRuntime for CodeModeWaitHandler {
    fn pre_tool_use_payload(&self, _invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        // Code-mode `wait` is runtime control for an existing code cell, not a
        // standalone user action. Tool calls made from code mode still flow
        // through normal dispatch, but hooks should not block or rewrite the
        // wait loop itself.
        None
    }

    fn post_tool_use_payload(
        &self,
        _invocation: &ToolInvocation,
        _result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        // The wait result feeds code-mode control flow, so do not let
        // PostToolUse replace it with model-facing hook feedback.
        None
    }
}
