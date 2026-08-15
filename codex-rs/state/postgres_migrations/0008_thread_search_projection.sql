CREATE TABLE thread_search_content (
    thread_id TEXT NOT NULL REFERENCES threads(thread_id) ON DELETE CASCADE,
    rollout_ordinal BIGINT NOT NULL CHECK (rollout_ordinal >= 0),
    content TEXT NOT NULL,
    folded_content TEXT NOT NULL,
    normalized_content TEXT NOT NULL,
    normalized_folded_content TEXT NOT NULL,
    PRIMARY KEY (thread_id, rollout_ordinal)
);

UPDATE threads SET history_projection_version = NULL;
