CREATE TABLE thread_turns (
    thread_id TEXT NOT NULL REFERENCES threads(thread_id) ON DELETE CASCADE,
    turn_id TEXT NOT NULL,
    rollout_ordinal BIGINT NOT NULL CHECK (rollout_ordinal >= 0),
    status TEXT NOT NULL CHECK (status IN ('completed', 'interrupted', 'failed', 'inProgress')),
    error JSONB,
    started_at BIGINT,
    completed_at BIGINT,
    duration_ms BIGINT,
    PRIMARY KEY (thread_id, turn_id),
    UNIQUE (thread_id, rollout_ordinal)
);
