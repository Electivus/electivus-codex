use codex_state::MemoryStore;
use codex_state::Stage1Output;
use std::path::Path;

pub(crate) async fn complete_phase2(
    store: &MemoryStore,
    ownership_token: &str,
    completed_watermark: i64,
    selected_outputs: &[Stage1Output],
    memory_root: &Path,
) -> anyhow::Result<bool> {
    let artifacts = crate::collect_memory_artifacts(memory_root).await?;
    store
        .complete_global_consolidation(
            ownership_token,
            completed_watermark,
            selected_outputs,
            &artifacts,
        )
        .await
}

#[cfg(test)]
#[path = "phase2_completion_tests.rs"]
mod tests;
