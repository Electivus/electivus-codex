use codex_protocol::DurableSandboxPolicy;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::TurnContextItem;
use codex_utils_path_uri::PathUri;

pub(crate) fn turn_context_item(
    turn_id: &str,
    cwd: PathUri,
    sandbox_policy: SandboxPolicy,
    model: &str,
) -> TurnContextItem {
    let sandbox_policy: DurableSandboxPolicy = sandbox_policy.into();
    serde_json::from_value(serde_json::json!({
        "turn_id": turn_id,
        "cwd": cwd,
        "approval_policy": AskForApproval::Never,
        "sandbox_policy": sandbox_policy,
        "model": model,
        "summary": ReasoningSummary::Auto,
    }))
    .expect("test turn context should deserialize with optional fields defaulted")
}
