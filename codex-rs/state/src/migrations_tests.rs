use codex_utils_absolute_path::test_support::PathExt;
use sqlx::Connection;
use sqlx::Row;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;
use std::borrow::Cow;

use super::STATE_MIGRATOR;
use super::THREAD_HISTORY_MIGRATOR;
use super::repair_legacy_state_migration_versions;

pub(crate) fn repository_identity_cases() -> Vec<(String, Option<String>)> {
    let boundary_repo = "r".repeat(1009);
    let boundary_origin = format!("https://example.test/o/{boundary_repo}");
    let boundary_identity = format!("example.test/o/{boundary_repo}");
    assert_eq!(boundary_identity.len(), 1024);
    let utf8_boundary_repo = format!("{}r", "é".repeat(504));
    let utf8_boundary_origin = format!("https://example.test/o/{utf8_boundary_repo}");
    let utf8_boundary_identity = format!("example.test/o/{utf8_boundary_repo}");
    assert_eq!(utf8_boundary_identity.len(), 1024);

    vec![
        (
            "git@github.com:OpenAI/Codex.git".to_string(),
            Some("github.com/openai/codex".to_string()),
        ),
        (
            "https://user@token@github.com/OpenAI/Codex.git".to_string(),
            Some("github.com/openai/codex".to_string()),
        ),
        (
            " https://token@GitHub.com /OpenAI/Codex.git ".to_string(),
            Some("github.com/openai/codex".to_string()),
        ),
        (
            "\t\nhttps://github.com/OpenAI/Codex.git\r\n".to_string(),
            Some("github.com/openai/codex".to_string()),
        ),
        (
            "\u{2003}https://github.com/OpenAI/Codex.git\u{3000}".to_string(),
            Some("github.com/openai/codex".to_string()),
        ),
        (
            "ssh://git@github.com:22/OpenAI/Codex.git".to_string(),
            Some("github.com/openai/codex".to_string()),
        ),
        (
            "ssh://git@ghe.example.test:2222/Org/Repo.git".to_string(),
            Some("ghe.example.test:2222/Org/Repo".to_string()),
        ),
        (
            "https://github.com/Örg/RÉpo.git".to_string(),
            Some("github.com/Örg/rÉpo".to_string()),
        ),
        (
            "https://github.com/OpenAI/Codex.git?ref=main".to_string(),
            Some("github.com/openai/codex".to_string()),
        ),
        (
            "https://ghe.example.test/Org/Repo://Mirror.git".to_string(),
            Some("ghe.example.test/Org/Repo:/Mirror".to_string()),
        ),
        (boundary_origin.clone(), Some(boundary_identity)),
        (format!("{boundary_origin}r"), None),
        (utf8_boundary_origin.clone(), Some(utf8_boundary_identity)),
        (format!("{utf8_boundary_origin}r"), None),
        ("https://example.test/acme/repo\ninjected".to_string(), None),
        (
            "https://example.test/acme/repo\u{2003}injected".to_string(),
            None,
        ),
        ("https://example.test/repo.git".to_string(), None),
    ]
}

fn migrator_through(version: i64) -> Migrator {
    Migrator {
        migrations: Cow::Owned(
            STATE_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version <= version)
                .cloned()
                .collect(),
        ),
        ignore_missing: STATE_MIGRATOR.ignore_missing,
        locking: STATE_MIGRATOR.locking,
        table_name: STATE_MIGRATOR.table_name.clone(),
        create_schemas: STATE_MIGRATOR.create_schemas.clone(),
        no_tx: STATE_MIGRATOR.no_tx,
    }
}

#[test]
fn repository_identity_corpus_matches_rust_canonicalizer() {
    for (origin, expected) in repository_identity_cases() {
        assert_eq!(
            codex_git_utils::canonicalize_git_remote_url(&origin),
            expected,
            "origin: {origin:?}"
        );
    }
}

#[tokio::test]
async fn pinned_threads_migration_defaults_existing_and_legacy_rows_to_unpinned() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = sqlite.state_db_path();
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("sqlite database should open");
    migrator_through(/*version*/ 42)
        .run(&pool)
        .await
        .expect("pre-pin migrations should apply");

    for thread_id in [
        "00000000-0000-0000-0000-000000000043",
        "00000000-0000-0000-0000-000000000044",
    ] {
        if thread_id.ends_with("44") {
            STATE_MIGRATOR
                .run(&pool)
                .await
                .expect("pin migration should apply");
        }
        sqlx::query(
            r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    created_at_ms,
    updated_at_ms,
    source,
    model_provider,
    cwd,
    title,
    sandbox_policy,
    approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(thread_id)
        .bind("/tmp/legacy.jsonl")
        .bind(1_700_000_000_i64)
        .bind(1_700_000_000_i64)
        .bind(1_700_000_000_000_i64)
        .bind(1_700_000_000_000_i64)
        .bind("cli")
        .bind("openai")
        .bind("/tmp")
        .bind("")
        .bind("read-only")
        .bind("on-request")
        .execute(&pool)
        .await
        .expect("legacy thread insert should succeed");
    }

    let pinned_values = sqlx::query_scalar::<_, bool>("SELECT is_pinned FROM threads ORDER BY id")
        .fetch_all(&pool)
        .await
        .expect("pin states should load");
    assert_eq!(pinned_values, vec![false, false]);

    pool.close().await;
}

#[tokio::test]
async fn thread_item_update_ordinals_allow_older_writers() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let pre_update_ordinal_migrator = Migrator {
        migrations: Cow::Owned(
            THREAD_HISTORY_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version < 4)
                .cloned()
                .collect(),
        ),
        ignore_missing: THREAD_HISTORY_MIGRATOR.ignore_missing,
        locking: THREAD_HISTORY_MIGRATOR.locking,
        table_name: THREAD_HISTORY_MIGRATOR.table_name.clone(),
        create_schemas: THREAD_HISTORY_MIGRATOR.create_schemas.clone(),
        no_tx: THREAD_HISTORY_MIGRATOR.no_tx,
    };
    let pool = sqlite
        .open_thread_history_db(
            &pre_update_ordinal_migrator,
            /*telemetry_override*/ None,
        )
        .await
        .expect("pre-update-ordinal migrations should apply");
    sqlx::query(
        r#"
INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_type, item_json) VALUES
    ('thread-1', 'turn-1', 'existing-item-1', 11, 1_100, 'userMessage', '{}'),
    ('thread-1', 'turn-1', 'existing-item-2', 12, 1_200, 'userMessage', '{}')
        "#,
    )
    .execute(&pool)
    .await
    .expect("pre-migration items should be inserted");
    THREAD_HISTORY_MIGRATOR
        .run(&pool)
        .await
        .expect("update-ordinal migration should apply");
    sqlx::query(
        r#"
INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_type, item_json) VALUES
    ('thread-1', 'turn-1', 'old-writer-item-1', 13, 1_300, 'userMessage', '{}'),
    ('thread-1', 'turn-1', 'old-writer-item-2', 14, 1_400, 'userMessage', '{}')
        "#,
    )
    .execute(&pool)
    .await
    .expect("older writers should be able to append multiple items after migration");
    let ordinals = sqlx::query_as::<_, (i64, i64)>(
        "SELECT rollout_ordinal, updated_at_ordinal FROM thread_items WHERE thread_id = ? ORDER BY rollout_ordinal",
    )
    .bind("thread-1")
    .fetch_all(&pool)
    .await
    .expect("old-writer items should load");
    assert_eq!(ordinals, vec![(11, 11), (12, 12), (13, 0), (14, 0)]);

    pool.close().await;
}

#[tokio::test]
async fn agent_job_tables_are_dropped_when_upgrading() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = sqlite.state_db_path();
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("sqlite database should open");
    migrator_through(/*version*/ 15)
        .run(&pool)
        .await
        .expect("agent job migrations should apply");

    sqlx::query(
        r#"
INSERT INTO agent_jobs (
    id,
    name,
    status,
    instruction,
    input_headers_json,
    input_csv_path,
    output_csv_path,
    created_at,
    updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("job-1")
    .bind("legacy job")
    .bind("running")
    .bind("process rows")
    .bind(r#"["path"]"#)
    .bind("/tmp/input.csv")
    .bind("/tmp/output.csv")
    .bind(1_700_000_000_i64)
    .bind(1_700_000_000_i64)
    .execute(&pool)
    .await
    .expect("legacy agent job should insert");
    sqlx::query(
        r#"
INSERT INTO agent_job_items (
    job_id,
    item_id,
    row_index,
    row_json,
    status,
    result_json,
    created_at,
    updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("job-1")
    .bind("item-1")
    .bind(0_i64)
    .bind(r#"{"path":"secret.csv"}"#)
    .bind("completed")
    .bind(r#"{"result":"legacy"}"#)
    .bind(1_700_000_000_i64)
    .bind(1_700_000_000_i64)
    .execute(&pool)
    .await
    .expect("legacy agent job item should insert");

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply");

    let agent_job_tables = sqlx::query_scalar::<_, String>(
        r#"
SELECT name
FROM sqlite_master
WHERE type = 'table' AND name IN ('agent_jobs', 'agent_job_items')
ORDER BY name
        "#,
    )
    .fetch_all(&pool)
    .await
    .expect("remaining agent job tables should load");
    assert_eq!(agent_job_tables, Vec::<String>::new());

    pool.close().await;
}

#[tokio::test]
async fn recency_migration_backfills_and_seeds_old_binary_inserts() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = sqlite.state_db_path();
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("sqlite database should open");
    migrator_through(/*version*/ 37)
        .run(&pool)
        .await
        .expect("pre-recency migrations should apply");

    sqlx::query(
        r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    created_at_ms,
    updated_at_ms,
    source,
    model_provider,
    cwd,
    title,
    sandbox_policy,
    approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000001")
    .bind("/tmp/first.jsonl")
    .bind(1_700_000_000_i64)
    .bind(1_700_000_100_i64)
    .bind(1_700_000_000_123_i64)
    .bind(1_700_000_100_456_i64)
    .bind("cli")
    .bind("openai")
    .bind("/tmp")
    .bind("")
    .bind("read-only")
    .bind("on-request")
    .execute(&pool)
    .await
    .expect("legacy row should insert");

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("recency migration should apply");

    let backfilled = sqlx::query(
        "SELECT updated_at, updated_at_ms, recency_at, recency_at_ms FROM threads WHERE id = ?",
    )
    .bind("00000000-0000-0000-0000-000000000001")
    .fetch_one(&pool)
    .await
    .expect("backfilled row should load");
    assert_eq!(backfilled.get::<i64, _>("recency_at"), 1_700_000_100);
    assert_eq!(backfilled.get::<i64, _>("recency_at_ms"), 1_700_000_100_456);

    sqlx::query(
        r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    created_at_ms,
    updated_at_ms,
    source,
    model_provider,
    cwd,
    title,
    sandbox_policy,
    approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000002")
    .bind("/tmp/second.jsonl")
    .bind(1_700_000_200_i64)
    .bind(1_700_000_300_i64)
    .bind(1_700_000_200_123_i64)
    .bind(1_700_000_300_456_i64)
    .bind("cli")
    .bind("openai")
    .bind("/tmp")
    .bind("")
    .bind("read-only")
    .bind("on-request")
    .execute(&pool)
    .await
    .expect("old-binary row should insert");

    let seeded = sqlx::query("SELECT recency_at, recency_at_ms FROM threads WHERE id = ?")
        .bind("00000000-0000-0000-0000-000000000002")
        .fetch_one(&pool)
        .await
        .expect("old-binary row should load");
    assert_eq!(seeded.get::<i64, _>("recency_at"), 1_700_000_300);
    assert_eq!(seeded.get::<i64, _>("recency_at_ms"), 1_700_000_300_456);

    pool.close().await;
}

#[tokio::test]
async fn repository_identity_migration_backfills_valid_origins_without_changing_metadata() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("sqlite database should open");
    migrator_through(/*version*/ 45)
        .run(&pool)
        .await
        .expect("pre-identity migrations should apply");

    let cases = repository_identity_cases();
    for (index, (origin, _)) in cases.iter().enumerate() {
        let id = format!("00000000-0000-0000-0000-{:012}", index + 71);
        let title = format!("canonicalization-case-{index}");
        sqlx::query(
            r#"
INSERT INTO threads (
    id, rollout_path, created_at, updated_at, created_at_ms, updated_at_ms, source,
    model_provider, cwd, title, sandbox_policy, approval_mode, git_origin_url
) VALUES (?, ?, 1700000000, 1700000100, 1700000000000, 1700000100000, 'cli',
          'openai', '/tmp/project', ?, 'read-only', 'on-request', ?)
            "#,
        )
        .bind(&id)
        .bind(format!("/tmp/{id}.jsonl"))
        .bind(&title)
        .bind(origin)
        .execute(&pool)
        .await
        .expect("historical thread should insert");
    }

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("repository identity migration should apply");

    let rows = sqlx::query_as::<_, (String, String, String, Option<String>)>(
        "SELECT id, title, git_origin_url, repository_identity FROM threads ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("migrated threads should load");
    let expected_rows = cases
        .iter()
        .enumerate()
        .map(|(index, (origin, expected))| {
            (
                format!("00000000-0000-0000-0000-{:012}", index + 71),
                format!("canonicalization-case-{index}"),
                origin.clone(),
                expected.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(rows, expected_rows);

    pool.close().await;
}

#[tokio::test]
async fn repairs_recency_migration_that_was_applied_as_version_38() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = sqlite.state_db_path();
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("sqlite database should open");
    migrator_through(/*version*/ 37)
        .run(&pool)
        .await
        .expect("pre-recency migrations should apply");

    let recency_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == 39)
        .expect("recency migration should exist");
    let mut legacy_migrations = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version <= 37)
        .cloned()
        .collect::<Vec<_>>();
    legacy_migrations.push(Migration::new(
        38,
        recency_migration.description.clone(),
        recency_migration.migration_type,
        recency_migration.sql.clone(),
        recency_migration.no_tx,
    ));
    let legacy_recency_migrator = Migrator::with_migrations(legacy_migrations);
    legacy_recency_migrator
        .run(&pool)
        .await
        .expect("legacy recency migration should apply as version 38");

    repair_legacy_state_migration_versions(&pool, &STATE_MIGRATOR)
        .await
        .expect("legacy migration history should be repaired");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply after repair");

    let applied = sqlx::query(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version >= 38 ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .expect("applied migrations should load")
    .into_iter()
    .map(|row| {
        (
            row.get::<i64, _>("version"),
            row.get::<Vec<u8>, _>("checksum"),
        )
    })
    .collect::<Vec<_>>();
    let expected = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version >= 38)
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(applied, expected);

    pool.close().await;
}

#[tokio::test]
async fn repairs_official_pin_and_provider_migrations_before_fork_migration() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = sqlite.state_db_path();
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("sqlite database should open");

    let pin_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == 44)
        .expect("pin migration should exist");
    let provider_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == 45)
        .expect("provider migration should exist");
    let mut official_migrations = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version <= 42)
        .cloned()
        .collect::<Vec<_>>();
    official_migrations.push(Migration::new(
        /*version*/ 43,
        pin_migration.description.clone(),
        pin_migration.migration_type,
        pin_migration.sql.clone(),
        pin_migration.no_tx,
    ));
    official_migrations.push(Migration::new(
        /*version*/ 44,
        provider_migration.description.clone(),
        provider_migration.migration_type,
        provider_migration.sql.clone(),
        provider_migration.no_tx,
    ));
    Migrator::with_migrations(official_migrations)
        .run(&pool)
        .await
        .expect("official migrations should apply");

    repair_legacy_state_migration_versions(&pool, &STATE_MIGRATOR)
        .await
        .expect("official migration history should be repaired");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("fork migrations should apply after repair");

    let applied = sqlx::query(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version >= 43 ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .expect("applied migrations should load")
    .into_iter()
    .map(|row| {
        (
            row.get::<i64, _>("version"),
            row.get::<Vec<u8>, _>("checksum"),
        )
    })
    .collect::<Vec<_>>();
    let expected = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version >= 43)
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(applied, expected);

    pool.close().await;
}

#[tokio::test]
async fn repair_recency_migration_succeeds_while_another_connection_holds_writer_slot() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = sqlite.state_db_path();
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("database should open");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply");
    let read_pool = sqlite
        .open_read_only_pool(&state_path)
        .await
        .expect("read-only pool should open");
    let mut write_connection = pool.acquire().await.expect("write connection should open");
    let write_transaction = write_connection
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("write transaction should acquire the writer slot");

    let repair_result = repair_legacy_state_migration_versions(&read_pool, &STATE_MIGRATOR).await;

    write_transaction
        .rollback()
        .await
        .expect("write transaction should roll back");
    drop(write_connection);
    read_pool.close().await;
    pool.close().await;
    repair_result.expect("current migration history should not need the writer slot");
}
