use super::collect_memory_artifacts;
use codex_state::MemoryArtifact;
use codex_state::MemoryArtifactSet;
use pretty_assertions::assert_eq;
use std::fs;
use tempfile::tempdir;

#[tokio::test]
async fn collector_returns_complete_deterministic_artifact_set() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let root = temp.path().join("memories");
    fs::create_dir_all(root.join(".git/objects"))?;
    fs::create_dir_all(root.join("extensions/chronicle/resources"))?;
    fs::create_dir_all(root.join("rollout_summaries"))?;
    fs::create_dir_all(root.join("skills/example"))?;
    fs::write(root.join(".git/HEAD"), b"local baseline metadata")?;
    fs::write(
        root.join("phase2_workspace_diff.md"),
        b"temporary prompt artifact",
    )?;
    fs::write(root.join("MEMORY.md"), b"# Memory\n")?;
    fs::write(root.join("future-artifact.bin"), b"future\0artifact")?;
    fs::write(root.join("memory_summary.md"), b"v1\n\nsummary\n")?;
    fs::write(root.join("raw_memories.md"), b"raw\n")?;
    fs::write(
        root.join("rollout_summaries/thread.md"),
        b"rollout summary\n",
    )?;
    fs::write(
        root.join("extensions/chronicle/resources/signal.md"),
        b"signal\n",
    )?;
    fs::write(root.join("skills/example/SKILL.md"), b"# Skill\n")?;

    let artifacts = collect_memory_artifacts(&root).await?;

    assert_eq!(
        artifacts,
        MemoryArtifactSet::new(vec![
            MemoryArtifact::new("MEMORY.md", b"# Memory\n".to_vec())?,
            MemoryArtifact::new(
                "extensions/chronicle/resources/signal.md",
                b"signal\n".to_vec(),
            )?,
            MemoryArtifact::new("future-artifact.bin", b"future\0artifact".to_vec())?,
            MemoryArtifact::new("memory_summary.md", b"v1\n\nsummary\n".to_vec())?,
            MemoryArtifact::new("raw_memories.md", b"raw\n".to_vec())?,
            MemoryArtifact::new("rollout_summaries/thread.md", b"rollout summary\n".to_vec(),)?,
            MemoryArtifact::new("skills/example/SKILL.md", b"# Skill\n".to_vec())?,
        ])?
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn collector_rejects_symlinks_without_returning_partial_snapshot() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let root = temp.path().join("memories");
    let outside = temp.path().join("outside.md");
    fs::create_dir_all(&root)?;
    fs::write(root.join("MEMORY.md"), b"safe\n")?;
    fs::write(&outside, b"must not be collected\n")?;
    std::os::unix::fs::symlink(&outside, root.join("escaped.md"))?;

    let error = collect_memory_artifacts(&root)
        .await
        .expect_err("symlinked artifact should reject the entire snapshot");

    assert!(error.to_string().contains("symlink"));
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn collector_rejects_case_insensitive_path_collisions() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let root = temp.path().join("memories");
    fs::create_dir_all(&root)?;
    fs::write(root.join("MEMORY.md"), b"first\n")?;
    fs::write(root.join("memory.MD"), b"second\n")?;

    let error = collect_memory_artifacts(&root)
        .await
        .expect_err("portable artifact keys must remain unique after case folding");

    assert!(error.to_string().contains("case-insensitive collision"));
    Ok(())
}
