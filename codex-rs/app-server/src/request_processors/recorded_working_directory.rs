use codex_utils_path_uri::LegacyAppPathString;
use std::path::Path;
use std::path::PathBuf;

/// Returns a host-native execution cwd only when the Recorded Working Directory is usable locally.
pub(super) fn execution_cwd_from_recorded_working_directory(
    recorded_working_directory: &Path,
    operation: &str,
) -> Option<PathBuf> {
    let recorded_working_directory = LegacyAppPathString::from_path(recorded_working_directory);
    let Some(execution_cwd) = recorded_working_directory.to_inferred_abs_path() else {
        tracing::warn!(
            operation,
            recorded_working_directory = recorded_working_directory.as_str(),
            "ignoring foreign or unusable Recorded Working Directory for execution"
        );
        return None;
    };
    Some(execution_cwd.into_path_buf())
}
