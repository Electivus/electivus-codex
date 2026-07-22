/// One immutable file-like value within a complete Memory Generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryArtifact {
    /// Portable path key relative to a disposable memory workspace.
    path: String,
    contents: Vec<u8>,
}

/// A complete, deterministically ordered artifact set safe to materialize on supported platforms.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryArtifactSet {
    artifacts: Vec<MemoryArtifact>,
}

impl MemoryArtifactSet {
    /// Validates cross-filesystem key uniqueness and orders artifacts by their portable keys.
    pub fn new(mut artifacts: Vec<MemoryArtifact>) -> anyhow::Result<Self> {
        let mut case_folded_keys = std::collections::BTreeSet::new();
        for artifact in &artifacts {
            let case_folded_key: String =
                artifact.path.chars().flat_map(char::to_uppercase).collect();
            anyhow::ensure!(
                case_folded_keys.insert(case_folded_key),
                "Memory Artifact set contains a case-insensitive collision at {}",
                artifact.path
            );
        }
        artifacts.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(Self { artifacts })
    }

    /// Returns the complete artifact set in portable path order.
    pub fn artifacts(&self) -> &[MemoryArtifact] {
        &self.artifacts
    }
}

impl MemoryArtifact {
    /// Creates an artifact after validating its portable, workspace-relative path key.
    pub fn new(path: impl Into<String>, contents: Vec<u8>) -> anyhow::Result<Self> {
        let path = path.into();
        anyhow::ensure!(!path.is_empty(), "Memory Artifact path must not be empty");
        for component in path.split('/') {
            let stem = component.split('.').next().unwrap_or(component);
            let uppercase_stem = stem.to_ascii_uppercase();
            let numbered_device = uppercase_stem
                .strip_prefix("COM")
                .or_else(|| uppercase_stem.strip_prefix("LPT"))
                .is_some_and(|number| matches!(number.as_bytes(), [b'1'..=b'9']));
            let reserved_device =
                matches!(uppercase_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered_device;
            let invalid_character = component.chars().any(|character| {
                character <= '\u{1f}'
                    || matches!(character, '<' | '>' | '"' | '|' | '?' | '*' | ':' | '\\')
            });
            anyhow::ensure!(
                !component.is_empty()
                    && component != "."
                    && component != ".."
                    && !component.ends_with([' ', '.'])
                    && !invalid_character
                    && !reserved_device,
                "Memory Artifact path must use portable relative components"
            );
        }
        Ok(Self { path, contents })
    }

    /// Returns the portable path key relative to the memory workspace.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the exact artifact bytes stored in the generation.
    pub fn contents(&self) -> &[u8] {
        &self.contents
    }
}

/// Complete Memory Artifact snapshot published by one successful consolidation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryGeneration {
    generation_id: String,
    completed_watermark: i64,
    published_at: i64,
    artifacts: MemoryArtifactSet,
}

impl MemoryGeneration {
    pub(crate) fn new(
        generation_id: String,
        completed_watermark: i64,
        published_at: i64,
        artifacts: MemoryArtifactSet,
    ) -> Self {
        Self {
            generation_id,
            completed_watermark,
            published_at,
            artifacts,
        }
    }

    /// Returns the immutable identifier assigned when the generation was published.
    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    /// Returns the phase-two input watermark completed by this generation.
    pub fn completed_watermark(&self) -> i64 {
        self.completed_watermark
    }

    /// Returns the generation publication time as Unix seconds.
    pub fn published_at(&self) -> i64 {
        self.published_at
    }

    /// Returns the generation's complete artifact set in portable path order.
    pub fn artifacts(&self) -> &[MemoryArtifact] {
        self.artifacts.artifacts()
    }
}

#[cfg(test)]
#[path = "generation_tests.rs"]
mod tests;
