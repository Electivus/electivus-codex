use anyhow::Context;
use codex_state::MemoryArtifact;
use codex_state::MemoryArtifactSet;
use std::fs;
use std::path::Path;

pub(crate) async fn collect_memory_artifacts(root: &Path) -> anyhow::Result<MemoryArtifactSet> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || collect_memory_artifacts_sync(&root)).await?
}

fn collect_memory_artifacts_sync(root: &Path) -> anyhow::Result<MemoryArtifactSet> {
    let root_metadata = fs::symlink_metadata(root)
        .with_context(|| format!("inspect memory workspace {}", root.display()))?;
    anyhow::ensure!(
        !root_metadata.file_type().is_symlink(),
        "memory workspace root must not be a symlink: {}",
        root.display()
    );
    anyhow::ensure!(
        root_metadata.is_dir(),
        "memory workspace root must be a directory: {}",
        root.display()
    );

    let mut pending = vec![(root.to_path_buf(), Vec::<String>::new())];
    let mut artifacts = Vec::new();
    while let Some((directory, parent_components)) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .with_context(|| format!("read memory workspace directory {}", directory.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| {
                format!("read memory workspace entry in {}", directory.display())
            })?;
            let path = entry.path();
            let name = entry.file_name().into_string().map_err(|_| {
                anyhow::anyhow!(
                    "memory workspace path contains a non-UTF-8 component: {}",
                    path.display()
                )
            })?;
            // These are the only workspace-local implementation artifacts. Every other regular
            // file belongs to the authoritative generation, including extension-defined files.
            if parent_components.is_empty()
                && (name == ".git" || name == crate::workspace_diff::FILENAME)
            {
                continue;
            }

            let mut components = parent_components.clone();
            components.push(name);
            let artifact_key = components.join("/");
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("inspect memory artifact {}", path.display()))?;
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "memory artifact must not be a symlink: {}",
                path.display()
            );
            if metadata.is_dir() {
                pending.push((path, components));
            } else if metadata.is_file() {
                let contents = fs::read(&path)
                    .with_context(|| format!("read memory artifact {}", path.display()))?;
                artifacts.push(MemoryArtifact::new(artifact_key, contents)?);
            } else {
                anyhow::bail!(
                    "memory workspace entry must be a regular file or directory: {}",
                    path.display()
                );
            }
        }
    }
    MemoryArtifactSet::new(artifacts)
}

#[cfg(test)]
#[path = "workspace_artifacts_tests.rs"]
mod tests;
