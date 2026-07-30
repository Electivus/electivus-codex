ALTER TABLE threads ADD COLUMN repository_identity TEXT;
ALTER TABLE threads ADD COLUMN git_origin_url_is_explicit INTEGER NOT NULL DEFAULT 0;

UPDATE threads
SET repository_identity = (
    WITH RECURSIVE
    whitespace(chars) AS (
        SELECT char(
            9, 10, 11, 12, 13, 32, 133, 160, 5760,
            8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199,
            8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288
        )
    ),
    trimmed(value) AS (
        SELECT rtrim(
            trim(git_origin_url, (SELECT chars FROM whitespace)),
            '/'
        )
    ),
    classified(scheme, rest, default_port, supported) AS (
        SELECT
            CASE
                WHEN instr(value, '://') > 0
                    THEN substr(value, 1, instr(value, '://') - 1)
                ELSE NULL
            END,
            CASE
                WHEN instr(value, '://') > 0
                    THEN substr(value, instr(value, '://') + 3)
                ELSE value
            END,
            CASE substr(value, 1, instr(value, '://') - 1)
                WHEN 'git' THEN '9418'
                WHEN 'http' THEN '80'
                WHEN 'https' THEN '443'
                WHEN 'ssh' THEN '22'
                ELSE NULL
            END,
            CASE
                WHEN instr(value, '://') = 0 THEN 1
                WHEN substr(value, 1, instr(value, '://') - 1) IN ('git', 'http', 'https', 'ssh')
                    THEN 1
                ELSE 0
            END
        FROM trimmed
    ),
    without_suffix(scheme, rest, default_port, supported) AS (
        SELECT
            scheme,
            CASE
                WHEN scheme IS NULL THEN rest
                WHEN instr(rest, '?') > 0 AND instr(rest, '#') > 0
                    THEN substr(rest, 1, min(instr(rest, '?'), instr(rest, '#')) - 1)
                WHEN instr(rest, '?') > 0 THEN substr(rest, 1, instr(rest, '?') - 1)
                WHEN instr(rest, '#') > 0 THEN substr(rest, 1, instr(rest, '#') - 1)
                ELSE rest
            END,
            default_port,
            supported
        FROM classified
    ),
    host_path(host_part, path, default_port, supported) AS (
        SELECT
            CASE
                WHEN scheme IS NOT NULL AND instr(rest, '/') > 0
                    THEN substr(rest, 1, instr(rest, '/') - 1)
                WHEN instr(rest, '/') > 0
                    AND (
                        instr(rest, ':') = 0
                        OR instr(rest, '/') < instr(rest, ':')
                    )
                    THEN substr(rest, 1, instr(rest, '/') - 1)
                WHEN instr(rest, ':') > 0 THEN substr(rest, 1, instr(rest, ':') - 1)
                ELSE ''
            END,
            CASE
                WHEN scheme IS NOT NULL AND instr(rest, '/') > 0
                    THEN substr(rest, instr(rest, '/') + 1)
                WHEN instr(rest, '/') > 0
                    AND (
                        instr(rest, ':') = 0
                        OR instr(rest, '/') < instr(rest, ':')
                    )
                    THEN substr(rest, instr(rest, '/') + 1)
                WHEN instr(rest, ':') > 0 THEN substr(rest, instr(rest, ':') + 1)
                ELSE ''
            END,
            default_port,
            supported
        FROM without_suffix
    ),
    credentials_stripped(host_part, path, default_port, supported) AS (
        SELECT
            trim(host_part, (SELECT chars FROM whitespace)),
            path,
            default_port,
            supported
        FROM host_path
        UNION ALL
        SELECT
            substr(host_part, instr(host_part, '@') + 1),
            path,
            default_port,
            supported
        FROM credentials_stripped
        WHERE instr(host_part, '@') > 0
    ),
    normalized_host(host, path, supported) AS (
        SELECT
            lower(CASE
                WHEN default_port IS NOT NULL AND lower(host_part) LIKE '%:' || default_port
                    THEN substr(host_part, 1, length(host_part) - length(default_port) - 1)
                ELSE host_part
            END),
            trim(trim(path, (SELECT chars FROM whitespace)), '/'),
            supported
        FROM credentials_stripped
        WHERE instr(host_part, '@') = 0
    ),
    collapsed_path(host, path, supported) AS (
        SELECT
            host,
            path,
            supported
        FROM normalized_host
        UNION ALL
        SELECT
            host,
            replace(path, '//', '/'),
            supported
        FROM collapsed_path
        WHERE instr(path, '//') > 0
    ),
    final_path(host, path, supported) AS (
        SELECT
            host,
            CASE
                WHEN substr(path, -4) = '.git'
                    THEN substr(path, 1, length(path) - 4)
                ELSE path
            END,
            supported
        FROM collapsed_path
        WHERE instr(path, '//') = 0
    ),
    components(host, path, owner, repo, supported) AS (
        SELECT
            host,
            path,
            substr(path, 1, instr(path, '/') - 1),
            CASE
                WHEN instr(substr(path, instr(path, '/') + 1), '/') > 0
                    THEN substr(
                        substr(path, instr(path, '/') + 1),
                        1,
                        instr(substr(path, instr(path, '/') + 1), '/') - 1)
                ELSE substr(path, instr(path, '/') + 1)
            END,
            supported
        FROM final_path
    ),
    candidate(identity) AS (
        SELECT
            CASE
                WHEN supported = 1
                    AND host <> ''
                    AND instr(path, '/') > 0
                    AND owner NOT IN ('', '.', '..')
                    AND repo NOT IN ('', '.', '..')
                    THEN host || '/' || CASE
                        WHEN host = 'github.com' THEN lower(path)
                        ELSE path
                    END
                ELSE NULL
            END
        FROM components
    ),
    identity_characters(identity, position, codepoint) AS (
        SELECT
            identity,
            1,
            unicode(substr(identity, 1, 1))
        FROM candidate
        WHERE identity IS NOT NULL
            AND length(CAST(identity AS BLOB)) <= 1024

        UNION ALL

        SELECT
            identity,
            position + 1,
            unicode(substr(identity, position + 1, 1))
        FROM identity_characters
        WHERE position < length(identity)
    )
    SELECT identity
    FROM candidate
    WHERE identity IS NOT NULL
        -- Repository Identity participates in composite PostgreSQL B-tree indexes too. A 1 KiB
        -- UTF-8 cap leaves conservative headroom for the remaining variable-width index columns.
        AND length(CAST(identity AS BLOB)) <= 1024
        AND instr(identity, char(0)) = 0
        AND NOT EXISTS (
            SELECT 1
            FROM identity_characters
            WHERE codepoint BETWEEN 0 AND 32
                OR codepoint BETWEEN 127 AND 160
                OR codepoint IN (
                    5760,
                    8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199,
                    8200, 8201, 8202,
                    8232, 8233, 8239, 8287, 12288
                )
        )
)
WHERE git_origin_url IS NOT NULL;

CREATE INDEX idx_threads_archived_repository_identity_created_at_ms
    ON threads(archived, repository_identity, created_at_ms DESC, id DESC);

CREATE INDEX idx_threads_archived_repository_identity_updated_at_ms
    ON threads(archived, repository_identity, updated_at_ms DESC, id DESC);

CREATE INDEX idx_threads_archived_repository_identity_recency_at_ms
    ON threads(archived, repository_identity, recency_at_ms DESC, id DESC);
