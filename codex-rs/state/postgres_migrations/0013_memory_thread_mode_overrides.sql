-- Canonical Thread History remains authoritative for explicit memory-mode changes.
-- A runtime pollution marker wins only over canonical memory-mode entries whose ordinal is
-- below polluted_at_stream_version. A later explicit ThreadStore update therefore supersedes it.
CREATE TABLE memory_thread_mode_overrides (
    thread_id TEXT PRIMARY KEY REFERENCES threads(thread_id) ON DELETE CASCADE,
    polluted_at_stream_version BIGINT NOT NULL CHECK (polluted_at_stream_version >= 0)
);
