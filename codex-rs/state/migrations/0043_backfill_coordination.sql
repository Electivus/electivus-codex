ALTER TABLE backfill_state ADD COLUMN owner_id TEXT;
ALTER TABLE backfill_state ADD COLUMN fencing_token INTEGER NOT NULL DEFAULT 0;
ALTER TABLE backfill_state ADD COLUMN lease_expires_at_ms INTEGER;
