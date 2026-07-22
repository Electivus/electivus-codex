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

/// Backend-neutral action for synchronizing the disposable local memory workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryWorkspaceMaterialization {
    /// Keep the filesystem-authoritative workspace unchanged.
    Preserve,
    /// Replace the local workspace with one complete authoritative generation.
    Replace {
        /// Immutable identity used to detect publication changes during materialization.
        generation_id: String,
        /// Complete artifact set published by the generation.
        artifacts: MemoryArtifactSet,
    },
    /// Replace the local workspace with an empty artifact set.
    Clear,
}

impl MemoryArtifactSet {
    /// Validates cross-filesystem key uniqueness and orders artifacts by their portable keys.
    pub fn new(mut artifacts: Vec<MemoryArtifact>) -> anyhow::Result<Self> {
        let case_folded_paths = artifacts
            .iter()
            .map(|artifact| {
                (
                    artifact
                        .path
                        .chars()
                        .flat_map(char::to_uppercase)
                        .collect::<String>(),
                    artifact.path.as_str(),
                )
            })
            .collect::<Vec<_>>();
        let mut case_folded_keys = std::collections::BTreeSet::new();
        for (case_folded_key, path) in &case_folded_paths {
            anyhow::ensure!(
                case_folded_keys.insert(case_folded_key.as_str()),
                "Memory Artifact set contains a case-insensitive collision at {path}"
            );
        }
        for (case_folded_key, path) in &case_folded_paths {
            for (separator_index, _) in case_folded_key.match_indices('/') {
                anyhow::ensure!(
                    !case_folded_keys.contains(&case_folded_key[..separator_index]),
                    "Memory Artifact set contains a file-directory collision at {path}"
                );
            }
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

    pub(crate) fn into_artifact_set(self) -> MemoryArtifactSet {
        self.artifacts
    }
}

#[cfg(test)]
#[path = "generation_tests.rs"]
mod tests;
