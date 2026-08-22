UPDATE threads
SET thread_section_id = '01984de2-8f74-7c91-a3b2-5c5e937cf318'
WHERE is_pinned = 1 AND thread_section_id IS NULL;

WITH section_max_positions AS (
    SELECT thread_section_id, COALESCE(MAX(section_position), 0) AS max_position
    FROM threads
    WHERE thread_section_id IS NOT NULL
    GROUP BY thread_section_id
), ranked AS (
    SELECT threads.id,
           section_max_positions.max_position + ROW_NUMBER() OVER (
               PARTITION BY threads.thread_section_id
               ORDER BY threads.recency_at_ms DESC, threads.id DESC
           ) * 1000000 AS position
    FROM threads
    JOIN section_max_positions USING (thread_section_id)
    WHERE threads.thread_section_id IS NOT NULL AND threads.section_position IS NULL
)
UPDATE threads
SET section_position = ranked.position,
    section_entered_at_ms = COALESCE(threads.section_entered_at_ms, threads.recency_at_ms)
FROM ranked
WHERE threads.id = ranked.id;
