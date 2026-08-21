CREATE TABLE thread_goals (
    thread_id TEXT PRIMARY KEY REFERENCES threads(thread_id) ON DELETE CASCADE,
    goal_id TEXT NOT NULL,
    objective TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'active',
        'paused',
        'blocked',
        'usage_limited',
        'budget_limited',
        'complete'
    )),
    token_budget BIGINT,
    tokens_used BIGINT NOT NULL DEFAULT 0 CHECK (tokens_used >= 0),
    time_used_seconds BIGINT NOT NULL DEFAULT 0 CHECK (time_used_seconds >= 0),
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL
);

CREATE TABLE thread_goal_continuation_deferrals (
    thread_id TEXT PRIMARY KEY REFERENCES thread_goals(thread_id) ON DELETE CASCADE
);
