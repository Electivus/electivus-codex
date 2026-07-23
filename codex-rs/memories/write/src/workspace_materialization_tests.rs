use super::STAGING_SUFFIX;
use super::materialize_memory_workspace;
use super::prepare_memory_workspace;
use super::prepare_memory_workspace_from_store;
use super::sibling_path;
use anyhow::Result;
use codex_state::MemoryArtifact;
use codex_state::MemoryArtifactSet;
use codex_state::MemoryWorkspaceMaterialization;
use codex_state::StateRuntime;
use pretty_assertions::assert_eq;
use std::collections::VecDeque;
use tempfile::TempDir;

fn replacement(artifacts: Vec<MemoryArtifact>) -> Result<MemoryWorkspaceMaterialization> {
    Ok(MemoryWorkspaceMaterialization::Replace {
        generation_id: "test-generation".to_string(),
        artifacts: MemoryArtifactSet::new(artifacts)?,
    })
}

fn versioned_replacement(
    generation_id: &str,
    contents: &[u8],
) -> Result<MemoryWorkspaceMaterialization> {
    Ok(MemoryWorkspaceMaterialization::Replace {
        generation_id: generation_id.to_string(),
        artifacts: MemoryArtifactSet::new(vec![MemoryArtifact::new(
            "MEMORY.md",
            contents.to_vec(),
        )?])?,
    })
}

#[tokio::test]
async fn preparation_retries_until_the_materialized_source_is_stable() -> Result<()> {
    let generation_a = versioned_replacement("generation-a", b"old")?;
    let generation_b = versioned_replacement("generation-b", b"new")?;
    let mut reads = VecDeque::from([
        generation_a.clone(),
        generation_b.clone(),
        generation_b.clone(),
    ]);
    let mut materialized = Vec::new();

    prepare_memory_workspace(
        || std::future::ready(Ok(reads.pop_front().expect("one queued source read"))),
        |materialization| {
            materialized.push(materialization);
            std::future::ready(Ok(()))
        },
    )
    .await?;

    assert_eq!(materialized, vec![generation_a, generation_b]);
    assert!(reads.is_empty());
    Ok(())
}

#[tokio::test]
async fn materialization_rebuilds_deleted_workspace_with_exact_nested_bytes() -> Result<()> {
    let home = TempDir::new()?;
    let root = home.path().join("memories");
    let binary_contents = b"exact\0bytes\xff".to_vec();

    materialize_memory_workspace(
        &root,
        replacement(vec![
            MemoryArtifact::new("MEMORY.md", b"# memory\n".to_vec())?,
            MemoryArtifact::new(
                "extensions/source/resources/data.bin",
                binary_contents.clone(),
            )?,
        ])?,
    )
    .await?;

    assert_eq!(
        tokio::fs::read(root.join("MEMORY.md")).await?,
        b"# memory\n"
    );
    assert_eq!(
        tokio::fs::read(root.join("extensions/source/resources/data.bin")).await?,
        binary_contents
    );
    Ok(())
}

#[tokio::test]
async fn materialization_replaces_generation_and_removes_old_files_and_git() -> Result<()> {
    let home = TempDir::new()?;
    let root = home.path().join("memories");
    materialize_memory_workspace(
        &root,
        replacement(vec![MemoryArtifact::new("old.md", b"old".to_vec())?])?,
    )
    .await?;
    tokio::fs::create_dir(root.join(".git")).await?;
    tokio::fs::write(root.join(".git/HEAD"), "local metadata").await?;
    tokio::fs::write(root.join("local-stale.md"), "stale").await?;

    materialize_memory_workspace(
        &root,
        replacement(vec![MemoryArtifact::new("new.md", b"new".to_vec())?])?,
    )
    .await?;

    assert_eq!(tokio::fs::read(root.join("new.md")).await?, b"new");
    assert!(!tokio::fs::try_exists(root.join("old.md")).await?);
    assert!(!tokio::fs::try_exists(root.join("local-stale.md")).await?);
    assert!(!tokio::fs::try_exists(root.join(".git")).await?);
    Ok(())
}

#[tokio::test]
async fn empty_authority_clears_stale_local_workspace() -> Result<()> {
    let home = TempDir::new()?;
    let root = home.path().join("memories");
    tokio::fs::create_dir(&root).await?;
    tokio::fs::write(root.join("stale.md"), "stale").await?;

    materialize_memory_workspace(&root, MemoryWorkspaceMaterialization::Clear).await?;

    let mut entries = tokio::fs::read_dir(&root).await?;
    assert!(entries.next_entry().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn filesystem_authority_preserves_existing_workspace() -> Result<()> {
    let home = TempDir::new()?;
    let root = home.path().join("memories");
    tokio::fs::create_dir(&root).await?;
    tokio::fs::write(root.join("MEMORY.md"), "sqlite memory").await?;

    let state =
        StateRuntime::init_sqlite(home.path().to_path_buf(), "test-provider".to_string()).await?;
    prepare_memory_workspace_from_store(state.memories(), &root).await?;

    assert_eq!(
        tokio::fs::read_to_string(root.join("MEMORY.md")).await?,
        "sqlite memory"
    );
    state.close().await;
    Ok(())
}

#[tokio::test]
async fn staging_failure_leaves_previous_workspace_without_mixture() -> Result<()> {
    let home = TempDir::new()?;
    let root = home.path().join("memories");
    tokio::fs::create_dir(&root).await?;
    tokio::fs::write(root.join("MEMORY.md"), "previous").await?;
    tokio::fs::write(sibling_path(&root, STAGING_SUFFIX)?, "not a directory").await?;

    materialize_memory_workspace(
        &root,
        replacement(vec![MemoryArtifact::new("new.md", b"new".to_vec())?])?,
    )
    .await
    .expect_err("an unsafe staging path must fail before swapping the root");

    assert_eq!(
        tokio::fs::read_to_string(root.join("MEMORY.md")).await?,
        "previous"
    );
    assert!(!tokio::fs::try_exists(root.join("new.md")).await?);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn materialization_rejects_symlinked_root_without_following_it() -> Result<()> {
    use std::os::unix::fs::symlink;

    let home = TempDir::new()?;
    let outside = home.path().join("outside");
    tokio::fs::create_dir(&outside).await?;
    tokio::fs::write(outside.join("keep.md"), "keep").await?;
    let root = home.path().join("memories");
    symlink(&outside, &root)?;

    materialize_memory_workspace(&root, MemoryWorkspaceMaterialization::Clear)
        .await
        .expect_err("symlinked root must fail closed");

    assert_eq!(
        tokio::fs::read_to_string(outside.join("keep.md")).await?,
        "keep"
    );
    Ok(())
}
