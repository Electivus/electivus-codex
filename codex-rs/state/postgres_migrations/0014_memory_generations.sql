CREATE TABLE memory_generations (
    generation_id UUID PRIMARY KEY,
    completed_watermark BIGINT NOT NULL,
    published_at BIGINT NOT NULL,
    artifact_count INTEGER NOT NULL CHECK (artifact_count >= 0),
    total_bytes BIGINT NOT NULL CHECK (total_bytes >= 0)
);

CREATE TABLE memory_generation_artifacts (
    generation_id UUID NOT NULL REFERENCES memory_generations(generation_id) ON DELETE CASCADE,
    artifact_path TEXT NOT NULL CHECK (artifact_path <> ''),
    contents BYTEA NOT NULL,
    PRIMARY KEY (generation_id, artifact_path)
);

CREATE TABLE memory_generation_state (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    active_generation_id UUID REFERENCES memory_generations(generation_id)
);

INSERT INTO memory_generation_state (singleton, active_generation_id)
VALUES (TRUE, NULL);
