ALTER TABLE threads ADD COLUMN section_position BIGINT;
ALTER TABLE threads ADD COLUMN section_entered_at TIMESTAMPTZ;

UPDATE threads
SET projection = jsonb_set(
        jsonb_set(projection, '{section_position}', 'null'::jsonb, TRUE),
        '{section_entered_at}',
        'null'::jsonb,
        TRUE
    );

WITH ranked AS (
    SELECT thread_id,
           ROW_NUMBER() OVER (
               PARTITION BY thread_section_id
               ORDER BY recency_at DESC, thread_id DESC
           ) * 1000000 AS position
    FROM threads
    WHERE thread_section_id IS NOT NULL
)
UPDATE threads AS target
SET section_position = ranked.position,
    section_entered_at = target.recency_at,
    projection = jsonb_set(
        jsonb_set(target.projection, '{section_position}', to_jsonb(ranked.position), TRUE),
        '{section_entered_at}',
        to_jsonb(target.recency_at),
        TRUE
    )
FROM ranked
WHERE target.thread_id = ranked.thread_id;

CREATE INDEX threads_section_position_idx
ON threads(thread_section_id, archived_at, section_position ASC, thread_id ASC)
WHERE thread_section_id IS NOT NULL AND COALESCE(projection ->> 'preview', '') <> '';
