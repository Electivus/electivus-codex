ALTER TABLE external_agent_config_imports
    ADD COLUMN memory_import_fingerprint BYTEA,
    ADD COLUMN memory_generation_id UUID;
