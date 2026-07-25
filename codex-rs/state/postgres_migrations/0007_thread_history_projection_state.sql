ALTER TABLE threads
ADD COLUMN history_projection_version BIGINT CHECK (history_projection_version >= 0);
