CREATE TABLE backfill_state (
    id SMALLINT PRIMARY KEY CHECK (id = 1),
    status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'complete')),
    last_watermark TEXT,
    last_success_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    owner_id TEXT,
    fencing_token BIGINT NOT NULL DEFAULT 0 CHECK (fencing_token >= 0),
    lease_expires_at TIMESTAMPTZ,
    CHECK (
        (status = 'running' AND owner_id IS NOT NULL AND lease_expires_at IS NOT NULL)
        OR
        (status IN ('pending', 'complete') AND owner_id IS NULL AND lease_expires_at IS NULL)
    )
);

INSERT INTO backfill_state (id, status) VALUES (1, 'pending');
