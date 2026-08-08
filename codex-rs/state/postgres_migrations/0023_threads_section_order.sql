ALTER TABLE threads ADD COLUMN section_position BIGINT;
ALTER TABLE threads ADD COLUMN section_entered_at TIMESTAMPTZ;

UPDATE threads
SET projection = jsonb_set(
        jsonb_set(projection, '{section_position}', 'null'::jsonb, TRUE),
        '{section_entered_at}',
        'null'::jsonb,
        TRUE
    );

CREATE INDEX threads_section_position_idx
ON threads(thread_section_id, archived_at, section_position ASC, thread_id ASC)
WHERE thread_section_id IS NOT NULL AND COALESCE(projection ->> 'preview', '') <> '';
