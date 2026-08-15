ALTER TABLE thread_history
ADD COLUMN source_ordinal BIGINT CHECK (source_ordinal >= 0);

CREATE TABLE runtime_state_migration (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    source_identity TEXT NOT NULL,
    source_fingerprint TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('threads_imported', 'operational_imported', 'memory_imported', 'ready')),
    ready BOOLEAN NOT NULL DEFAULT FALSE,
    phase_evidence JSONB NOT NULL DEFAULT '{}'::jsonb,
    fencing_token BIGINT NOT NULL CHECK (fencing_token > 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK ((ready AND phase = 'ready') OR (NOT ready AND phase <> 'ready'))
);

CREATE TABLE thread_spawn_edges (
    parent_thread_id TEXT NOT NULL REFERENCES threads(thread_id) ON DELETE CASCADE,
    child_thread_id TEXT PRIMARY KEY REFERENCES threads(thread_id) ON DELETE CASCADE,
    status TEXT NOT NULL CHECK (status IN ('open', 'closed'))
);

CREATE INDEX thread_spawn_edges_parent_status_idx
ON thread_spawn_edges(parent_thread_id, status);
