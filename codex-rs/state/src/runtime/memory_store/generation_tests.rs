use super::MemoryArtifact;
use super::MemoryArtifactSet;
use pretty_assertions::assert_eq;

#[test]
fn memory_artifact_accepts_portable_relative_key() -> anyhow::Result<()> {
    let artifact = MemoryArtifact::new("skills/example/SKILL.md", b"contents".to_vec())?;

    assert_eq!(
        artifact,
        MemoryArtifact {
            path: "skills/example/SKILL.md".to_string(),
            contents: b"contents".to_vec(),
        }
    );
    Ok(())
}

#[test]
fn memory_artifact_rejects_unsafe_or_ambiguous_keys() {
    for path in [
        "",
        "/MEMORY.md",
        "C:/MEMORY.md",
        "C:MEMORY.md",
        "skills//SKILL.md",
        "skills/./SKILL.md",
        "skills/../MEMORY.md",
        "skills\\example\\SKILL.md",
        "skills/example/",
        "skills/\0SKILL.md",
        "skills/name<.md",
        "skills/name>.md",
        "skills/name\".md",
        "skills/name|.md",
        "skills/name?.md",
        "skills/name*.md",
        "skills/control\u{1f}.md",
        "CON",
        "con.md",
        "skills/PrN.txt",
        "AUX",
        "nul.bin",
        "COM1",
        "com9.log",
        "LPT1",
        "lpt9.log",
        "skills/trailing-space ",
        "skills/trailing-dot.",
    ] {
        assert!(
            MemoryArtifact::new(path, Vec::new()).is_err(),
            "unsafe artifact key should be rejected: {path:?}"
        );
    }
}

#[test]
fn memory_artifact_accepts_names_outside_windows_reserved_ranges() -> anyhow::Result<()> {
    for path in ["COM10.md", "LPT0", "console.md", "skills/name .md"] {
        MemoryArtifact::new(path, Vec::new())?;
    }
    Ok(())
}

#[test]
fn memory_artifact_set_rejects_case_insensitive_path_collisions() -> anyhow::Result<()> {
    let error = MemoryArtifactSet::new(vec![
        MemoryArtifact::new("Skills/Example/SKILL.md", Vec::new())?,
        MemoryArtifact::new("skills/example/skill.MD", Vec::new())?,
    ])
    .expect_err("case-insensitive filesystems would collapse these artifacts");

    assert!(error.to_string().contains("case-insensitive collision"));
    Ok(())
}

#[test]
fn memory_artifact_set_uses_deterministic_unicode_case_fold() -> anyhow::Result<()> {
    let error = MemoryArtifactSet::new(vec![
        MemoryArtifact::new("straße.md", Vec::new())?,
        MemoryArtifact::new("STRASSE.md", Vec::new())?,
    ])
    .expect_err("Unicode case folds must not produce materialization collisions");

    assert!(error.to_string().contains("case-insensitive collision"));
    Ok(())
}

#[test]
fn memory_artifact_set_rejects_file_directory_prefix_collisions() -> anyhow::Result<()> {
    let error = MemoryArtifactSet::new(vec![
        MemoryArtifact::new("skills/example", Vec::new())?,
        MemoryArtifact::new("Skills/Example/SKILL.md", Vec::new())?,
    ])
    .expect_err("a portable key cannot be both a file and a directory");

    assert!(error.to_string().contains("file-directory collision"));
    Ok(())
}

#[test]
fn memory_artifact_set_rejects_non_adjacent_file_directory_prefix_collisions() -> anyhow::Result<()>
{
    let error = MemoryArtifactSet::new(vec![
        MemoryArtifact::new("A", Vec::new())?,
        MemoryArtifact::new("A-B", Vec::new())?,
        MemoryArtifact::new("a/B", Vec::new())?,
    ])
    .expect_err("an intervening key cannot hide a case-insensitive file-directory collision");

    assert!(error.to_string().contains("file-directory collision"));
    Ok(())
}

#[test]
fn memory_artifact_set_orders_distinct_portable_keys() -> anyhow::Result<()> {
    let artifacts = MemoryArtifactSet::new(vec![
        MemoryArtifact::new("skills/zeta/SKILL.md", Vec::new())?,
        MemoryArtifact::new("MEMORY.md", Vec::new())?,
    ])?;

    assert_eq!(
        artifacts
            .artifacts()
            .iter()
            .map(MemoryArtifact::path)
            .collect::<Vec<_>>(),
        vec!["MEMORY.md", "skills/zeta/SKILL.md"]
    );
    Ok(())
}
