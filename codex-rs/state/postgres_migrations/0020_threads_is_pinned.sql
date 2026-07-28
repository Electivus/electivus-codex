ALTER TABLE threads
ADD COLUMN is_pinned BOOLEAN NOT NULL DEFAULT FALSE;

UPDATE threads
SET is_pinned = COALESCE((projection ->> 'is_pinned')::BOOLEAN, FALSE);

CREATE INDEX threads_pinned_recency_idx
ON threads(is_pinned, archived_at, recency_at DESC, thread_id DESC)
WHERE COALESCE(projection ->> 'preview', '') <> '';
