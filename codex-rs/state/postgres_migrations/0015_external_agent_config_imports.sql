CREATE TABLE external_agent_config_imports (
    import_id TEXT PRIMARY KEY,
    completed_at_ms BIGINT NOT NULL,
    successes JSONB NOT NULL,
    failures JSONB NOT NULL
);

CREATE INDEX external_agent_config_imports_history_idx
    ON external_agent_config_imports (completed_at_ms DESC, import_id ASC);
