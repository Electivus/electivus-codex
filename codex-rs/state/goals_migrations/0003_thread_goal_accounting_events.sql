CREATE TABLE thread_goal_accounting_events (
    thread_id TEXT NOT NULL REFERENCES thread_goals(thread_id) ON DELETE CASCADE,
    event_id TEXT NOT NULL,
    goal_id TEXT NOT NULL,
    time_delta_seconds INTEGER NOT NULL CHECK(time_delta_seconds >= 0),
    token_delta INTEGER NOT NULL CHECK(token_delta >= 0),
    mode TEXT NOT NULL CHECK(mode IN ('active_status_only', 'active_only', 'active_or_complete', 'active_or_stopped')),
    PRIMARY KEY(thread_id, event_id)
);
