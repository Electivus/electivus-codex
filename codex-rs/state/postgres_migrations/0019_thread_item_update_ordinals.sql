ALTER TABLE thread_items
ADD COLUMN updated_at_ordinal BIGINT;

UPDATE thread_items
SET updated_at_ordinal = rollout_ordinal;

ALTER TABLE thread_items
ALTER COLUMN updated_at_ordinal SET NOT NULL;

ALTER TABLE thread_items
ADD CONSTRAINT thread_items_updated_at_ordinal_nonnegative
CHECK (updated_at_ordinal >= 0);

CREATE INDEX thread_items_thread_updated_at_ordinal_idx
ON thread_items(thread_id, updated_at_ordinal);
