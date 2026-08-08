CREATE TABLE memory_stage1_outputs (
    thread_id TEXT PRIMARY KEY REFERENCES threads(thread_id) ON DELETE CASCADE,
    source_updated_at BIGINT NOT NULL,
    raw_memory TEXT NOT NULL,
    rollout_summary TEXT NOT NULL,
    rollout_slug TEXT,
    generated_at BIGINT NOT NULL,
    usage_count BIGINT,
    last_usage BIGINT,
    selected_for_phase2 BOOLEAN NOT NULL DEFAULT FALSE,
    selected_for_phase2_source_updated_at BIGINT
);

CREATE INDEX memory_stage1_outputs_source_updated_at_idx
    ON memory_stage1_outputs(source_updated_at DESC, thread_id DESC);

CREATE TABLE memory_jobs (
    kind TEXT NOT NULL,
    job_key TEXT NOT NULL,
    -- Stage-one jobs set this for cascade; the namespace-global job leaves it null.
    thread_id TEXT REFERENCES threads(thread_id) ON DELETE CASCADE,
    status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'done', 'error')),
    worker_id TEXT,
    ownership_token TEXT,
    started_at BIGINT,
    finished_at BIGINT,
    lease_until BIGINT,
    retry_at BIGINT,
    retry_remaining BIGINT NOT NULL,
    last_error TEXT,
    input_watermark BIGINT,
    last_success_watermark BIGINT,
    PRIMARY KEY (kind, job_key),
    CHECK (
        (kind = 'memory_stage1' AND thread_id = job_key)
        OR (kind = 'memory_consolidate_global' AND thread_id IS NULL)
    )
);

CREATE INDEX memory_jobs_kind_status_retry_lease_idx
    ON memory_jobs(kind, status, retry_at, lease_until);

CREATE INDEX memory_jobs_thread_id_idx
    ON memory_jobs(thread_id) WHERE thread_id IS NOT NULL;
