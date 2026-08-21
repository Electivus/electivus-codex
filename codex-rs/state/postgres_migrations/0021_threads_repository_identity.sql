ALTER TABLE threads ADD COLUMN repository_identity TEXT;

CREATE FUNCTION codex_repository_identity_for_migration(origin TEXT)
RETURNS TEXT LANGUAGE plpgsql IMMUTABLE STRICT
AS $$
DECLARE
    whitespace TEXT :=
        chr(9) || chr(10) || chr(11) || chr(12) || chr(13) || chr(32)
        || chr(133) || chr(160) || chr(5760)
        || chr(8192) || chr(8193) || chr(8194) || chr(8195)
        || chr(8196) || chr(8197) || chr(8198) || chr(8199)
        || chr(8200) || chr(8201) || chr(8202)
        || chr(8232) || chr(8233) || chr(8239) || chr(8287) || chr(12288);
    value TEXT := rtrim(btrim(origin, whitespace), '/');
    scheme TEXT;
    rest TEXT;
    host_part TEXT;
    path TEXT;
    host TEXT;
    default_port TEXT;
    components TEXT[];
    identity TEXT;
    character_index INTEGER;
    character_code INTEGER;
BEGIN
    IF value = '' THEN
        RETURN NULL;
    END IF;

    IF strpos(value, '://') > 0 THEN
        scheme := split_part(value, '://', 1);
        IF scheme NOT IN ('git', 'http', 'https', 'ssh') THEN
            RETURN NULL;
        END IF;
        default_port := CASE scheme
            WHEN 'git' THEN '9418'
            WHEN 'http' THEN '80'
            WHEN 'https' THEN '443'
            WHEN 'ssh' THEN '22'
        END;
        rest := regexp_replace(
            substr(value, strpos(value, '://') + 3),
            '[?#].*$',
            ''
        );
        IF strpos(rest, '/') = 0 THEN
            RETURN NULL;
        END IF;
        host_part := split_part(rest, '/', 1);
        path := substr(rest, strpos(rest, '/') + 1);
    ELSIF strpos(value, ':') > 0
        AND (
            strpos(value, '/') = 0
            OR strpos(value, ':') < strpos(value, '/')
        )
    THEN
        host_part := split_part(value, ':', 1);
        path := substr(value, strpos(value, ':') + 1);
    ELSIF strpos(value, '/') > 0 THEN
        host_part := split_part(value, '/', 1);
        path := substr(value, strpos(value, '/') + 1);
    ELSE
        RETURN NULL;
    END IF;

    host := translate(
        regexp_replace(rtrim(btrim(host_part, whitespace), '/'), '^.*@', ''),
        'ABCDEFGHIJKLMNOPQRSTUVWXYZ',
        'abcdefghijklmnopqrstuvwxyz'
    );
    IF default_port IS NOT NULL AND host LIKE '%:' || default_port THEN
        host := left(host, -length(default_port) - 1);
    END IF;
    path := regexp_replace(btrim(btrim(path, whitespace), '/'), '/+', '/', 'g');
    path := regexp_replace(path, '\.git$', '');
    components := string_to_array(path, '/');
    IF host = ''
        OR cardinality(components) < 2
        OR components[1] IN ('', '.', '..')
        OR components[2] IN ('', '.', '..')
    THEN
        RETURN NULL;
    END IF;

    IF host = 'github.com' THEN
        path := translate(path, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz');
    END IF;
    identity := host || '/' || path;
    -- Repository Identity participates in composite B-tree indexes. A 1 KiB UTF-8 cap leaves
    -- conservative headroom for the remaining variable-width index columns.
    IF octet_length(identity) > 1024 THEN
        RETURN NULL;
    END IF;

    FOR character_index IN 1..char_length(identity) LOOP
        character_code := ascii(substr(identity, character_index, 1));
        IF character_code BETWEEN 0 AND 32
            OR character_code BETWEEN 127 AND 160
            OR character_code IN (
                5760,
                8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199,
                8200, 8201, 8202,
                8232, 8233, 8239, 8287, 12288
            )
        THEN
            RETURN NULL;
        END IF;
    END LOOP;

    RETURN identity;
END;
$$;

UPDATE threads
SET repository_identity = codex_repository_identity_for_migration(
        projection #>> '{git_info,repository_url}'
    ),
    projection = CASE
        WHEN codex_repository_identity_for_migration(
            projection #>> '{git_info,repository_url}'
        ) IS NULL
            THEN projection
        ELSE jsonb_set(
            projection,
            '{repository_identity}',
            to_jsonb(codex_repository_identity_for_migration(
                projection #>> '{git_info,repository_url}'
            )),
            TRUE
        )
    END;

DROP FUNCTION codex_repository_identity_for_migration(TEXT);

CREATE INDEX threads_repository_identity_created_idx
    ON threads(repository_identity, archived_at, created_at DESC, thread_id DESC);

CREATE INDEX threads_repository_identity_updated_idx
    ON threads(repository_identity, archived_at, updated_at DESC, thread_id DESC);

CREATE INDEX threads_repository_identity_recency_idx
    ON threads(repository_identity, archived_at, recency_at DESC, thread_id DESC);
