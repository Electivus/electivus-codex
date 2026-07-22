/// One immutable file-like value within a complete Memory Generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryArtifact {
    /// Portable path key relative to a disposable memory workspace.
    pub(crate) path: String,
    pub(crate) contents: Vec<u8>,
}

/// Complete Memory Artifact snapshot published by one successful consolidation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryGeneration {
    pub(crate) generation_id: String,
    pub(crate) completed_watermark: i64,
    pub(crate) published_at: i64,
    pub(crate) artifacts: Vec<MemoryArtifact>,
}
