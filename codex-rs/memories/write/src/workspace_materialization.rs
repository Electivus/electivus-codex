use anyhow::Context;
use codex_state::MemoryArtifact;
use codex_state::MemoryStore;
use codex_state::MemoryWorkspaceMaterialization;
use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

const STAGING_SUFFIX: &str = ".codex-staging";
const BACKUP_SUFFIX: &str = ".codex-backup";
static MATERIALIZATION_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Synchronizes a disposable local memory workspace with its authoritative store generation.
pub async fn prepare_memory_workspace_from_store(
    store: &MemoryStore,
    root: &Path,
) -> anyhow::Result<()> {
    prepare_memory_workspace(
        || store.memory_workspace_materialization(),
        |materialization| materialize_memory_workspace(root, materialization),
    )
    .await
}

async fn prepare_memory_workspace<Load, LoadFuture, Materialize, MaterializeFuture>(
    mut load: Load,
    mut materialize: Materialize,
) -> anyhow::Result<()>
where
    Load: FnMut() -> LoadFuture,
    LoadFuture: Future<Output = anyhow::Result<MemoryWorkspaceMaterialization>>,
    Materialize: FnMut(MemoryWorkspaceMaterialization) -> MaterializeFuture,
    MaterializeFuture: Future<Output = anyhow::Result<()>>,
{
    let _permit = MATERIALIZATION_PERMITS
        .acquire()
        .await
        .context("acquire memory workspace materialization permit")?;
    let mut materialization = load().await?;
    loop {
        materialize(materialization.clone()).await?;
        if matches!(materialization, MemoryWorkspaceMaterialization::Preserve) {
            return Ok(());
        }
        let current = load().await?;
        if current == materialization {
            return Ok(());
        }
        materialization = current;
    }
}

/// Applies one already-loaded action to the disposable local workspace.
///
/// Store-backed callers serialize loading and materialization through
/// [`prepare_memory_workspace_from_store`]. Replacement is prepared in a sibling directory before
/// the root is swapped. A path lookup sees the old tree, a brief missing root, or the new tree.
/// Readers that independently open multiple paths are not generation-pinned and may span a swap;
/// read-side pinning is outside this checkpoint.
pub(crate) async fn materialize_memory_workspace(
    root: &Path,
    materialization: MemoryWorkspaceMaterialization,
) -> anyhow::Result<()> {
    if matches!(&materialization, MemoryWorkspaceMaterialization::Preserve) {
        return Ok(());
    }
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || materialize_memory_workspace_sync(&root, materialization))
        .await?
}

fn materialize_memory_workspace_sync(
    root: &Path,
    materialization: MemoryWorkspaceMaterialization,
) -> anyhow::Result<()> {
    let artifacts = match &materialization {
        MemoryWorkspaceMaterialization::Preserve => return Ok(()),
        MemoryWorkspaceMaterialization::Replace { artifacts, .. } => artifacts.artifacts(),
        MemoryWorkspaceMaterialization::Clear => &[],
    };
    let parent = root
        .parent()
        .context("memory workspace must have a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create memory workspace parent {}", parent.display()))?;
    let staging = sibling_path(root, STAGING_SUFFIX)?;
    let backup = sibling_path(root, BACKUP_SUFFIX)?;

    let mut root_exists = validate_workspace_root(root)?;
    if path_exists(&backup)? {
        if root_exists {
            remove_workspace_tree(&backup)?;
        } else {
            fs::rename(&backup, root).with_context(|| {
                format!(
                    "restore memory workspace {} from {}",
                    root.display(),
                    backup.display()
                )
            })?;
            root_exists = true;
        }
    }
    remove_workspace_tree(&staging)?;
    fs::create_dir(&staging)
        .with_context(|| format!("create memory workspace staging {}", staging.display()))?;
    if let Err(error) = write_artifacts(&staging, artifacts) {
        let _ = remove_workspace_tree(&staging);
        return Err(error);
    }

    if root_exists {
        fs::rename(root, &backup).with_context(|| {
            format!(
                "move memory workspace {} to backup {}",
                root.display(),
                backup.display()
            )
        })?;
    }
    if let Err(swap_error) = fs::rename(&staging, root) {
        if root_exists && let Err(rollback_error) = fs::rename(&backup, root) {
            anyhow::bail!(
                "replace memory workspace {} failed: {swap_error}; rollback failed: {rollback_error}",
                root.display()
            );
        }
        return Err(swap_error)
            .with_context(|| format!("replace memory workspace {}", root.display()));
    }
    if root_exists {
        remove_workspace_tree(&backup)?;
    }
    Ok(())
}

fn sibling_path(root: &Path, suffix: &str) -> anyhow::Result<PathBuf> {
    let mut name = root
        .file_name()
        .context("memory workspace must have a final path component")?
        .to_os_string();
    name.push(OsString::from(suffix));
    Ok(root.with_file_name(name))
}

fn validate_workspace_root(root: &Path) -> anyhow::Result<bool> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "memory workspace root must be a non-symlink directory: {}",
                root.display()
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("inspect memory workspace root {}", root.display()))
        }
    }
}

fn path_exists(path: &Path) -> anyhow::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "memory workspace swap path must be a non-symlink directory: {}",
                path.display()
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("inspect workspace swap path {}", path.display()))
        }
    }
}

fn remove_workspace_tree(path: &Path) -> anyhow::Result<()> {
    if !path_exists(path)? {
        return Ok(());
    }
    fs::remove_dir_all(path)
        .with_context(|| format!("remove workspace swap directory {}", path.display()))
}

fn write_artifacts(staging: &Path, artifacts: &[MemoryArtifact]) -> anyhow::Result<()> {
    for artifact in artifacts {
        let mut target = staging.to_path_buf();
        let mut components = artifact.path().split('/').peekable();
        while let Some(component) = components.next() {
            target.push(component);
            if components.peek().is_some() {
                match fs::create_dir(&target) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let metadata = fs::symlink_metadata(&target).with_context(|| {
                            format!("inspect memory artifact directory {}", target.display())
                        })?;
                        anyhow::ensure!(
                            metadata.is_dir() && !metadata.file_type().is_symlink(),
                            "memory artifact parent must be a non-symlink directory: {}",
                            target.display()
                        );
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("create memory artifact directory {}", target.display())
                        });
                    }
                }
            } else {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .with_context(|| format!("create memory artifact {}", target.display()))?;
                file.write_all(artifact.contents())
                    .with_context(|| format!("write memory artifact {}", target.display()))?;
                file.sync_all()
                    .with_context(|| format!("sync memory artifact {}", target.display()))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "workspace_materialization_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "workspace_materialization_postgres_tests.rs"]
mod postgres_tests;
