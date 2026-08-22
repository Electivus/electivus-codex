ALTER TABLE thread_history
ADD COLUMN recorded_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP;

ALTER TABLE threads
ADD COLUMN history_projection_start_ordinal BIGINT CHECK (history_projection_start_ordinal >= 0);

CREATE TABLE thread_items (
    thread_id TEXT NOT NULL REFERENCES threads(thread_id) ON DELETE CASCADE,
    turn_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    rollout_ordinal BIGINT NOT NULL CHECK (rollout_ordinal >= 0),
    created_at_ms BIGINT NOT NULL,
    item JSONB NOT NULL,
    PRIMARY KEY (thread_id, turn_id, item_id),
    UNIQUE (thread_id, rollout_ordinal)
);

CREATE INDEX thread_items_thread_turn_ordinal_idx
ON thread_items(thread_id, turn_id, rollout_ordinal);
