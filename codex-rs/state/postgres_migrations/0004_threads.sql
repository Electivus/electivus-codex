CREATE TABLE threads (
    thread_id TEXT PRIMARY KEY,
    projection JSONB NOT NULL,
    stream_version BIGINT NOT NULL CHECK (stream_version >= 0),
    fencing_token BIGINT NOT NULL CHECK (fencing_token > 0),
    writer_id TEXT NOT NULL,
    writer_lease_expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    recency_at TIMESTAMPTZ NOT NULL,
    archived_at TIMESTAMPTZ
);

CREATE TABLE thread_history (
    thread_id TEXT NOT NULL REFERENCES threads(thread_id) ON DELETE CASCADE,
    ordinal BIGINT NOT NULL CHECK (ordinal >= 0),
    item JSONB NOT NULL,
    PRIMARY KEY (thread_id, ordinal)
);

CREATE TABLE thread_append_batches (
    thread_id TEXT NOT NULL REFERENCES threads(thread_id) ON DELETE CASCADE,
    idempotency_key TEXT NOT NULL,
    content_identity BYTEA NOT NULL,
    first_ordinal BIGINT NOT NULL CHECK (first_ordinal >= 0),
    item_count BIGINT NOT NULL CHECK (item_count > 0),
    committed_stream_version BIGINT NOT NULL CHECK (committed_stream_version > 0),
    committed_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (thread_id, idempotency_key)
);
