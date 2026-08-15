CREATE TABLE thread_sections (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL
);

INSERT INTO thread_sections (id, name)
VALUES ('01984de2-8f74-7c91-a3b2-5c5e937cf318', 'Pinned');

ALTER TABLE threads
ADD COLUMN thread_section_id TEXT
    REFERENCES thread_sections(id) ON DELETE SET NULL;

UPDATE threads
SET thread_section_id = '01984de2-8f74-7c91-a3b2-5c5e937cf318'
WHERE is_pinned = TRUE;

UPDATE threads
SET projection = jsonb_set(
    projection - 'is_pinned',
    '{section}',
    CASE
        WHEN is_pinned THEN jsonb_build_object(
            'id', '01984de2-8f74-7c91-a3b2-5c5e937cf318',
            'name', 'Pinned'
        )
        ELSE 'null'::jsonb
    END,
    TRUE
);

CREATE INDEX threads_section_recency_idx
ON threads(thread_section_id, archived_at, recency_at DESC, thread_id DESC)
WHERE thread_section_id IS NOT NULL AND COALESCE(projection ->> 'preview', '') <> '';
