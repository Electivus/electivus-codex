use std::borrow::Cow;

use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

pub(crate) static STATE_MIGRATOR: Migrator = sqlx::migrate!("./migrations");
pub(crate) static LOGS_MIGRATOR: Migrator = sqlx::migrate!("./logs_migrations");
pub(crate) static GOALS_MIGRATOR: Migrator = sqlx::migrate!("./goals_migrations");
pub(crate) static MEMORIES_MIGRATOR: Migrator = sqlx::migrate!("./memory_migrations");
pub(crate) static THREAD_HISTORY_MIGRATOR: Migrator = sqlx::migrate!("./thread_history_migrations");

/// Allow an older Codex binary to open a database that has already been
/// migrated by a newer binary running in parallel.
///
/// We intentionally ignore applied migration versions that are newer than the
/// embedded migration set. Known migration versions are still validated by
/// checksum, so this only relaxes the "database is ahead of me" case.
fn runtime_migrator(base: &'static Migrator) -> Migrator {
    Migrator {
        migrations: Cow::Borrowed(base.migrations.as_ref()),
        ignore_missing: true,
        locking: base.locking,
        no_tx: base.no_tx,
        table_name: base.table_name.clone(),
        create_schemas: base.create_schemas.clone(),
    }
}

pub(crate) fn runtime_state_migrator() -> Migrator {
    runtime_migrator(&STATE_MIGRATOR)
}

pub(crate) fn runtime_logs_migrator() -> Migrator {
    runtime_migrator(&LOGS_MIGRATOR)
}

pub(crate) fn runtime_goals_migrator() -> Migrator {
    runtime_migrator(&GOALS_MIGRATOR)
}

pub(crate) fn runtime_memories_migrator() -> Migrator {
    runtime_migrator(&MEMORIES_MIGRATOR)
}

// The paginated history projector will call this when it takes ownership of opening the database.
#[allow(dead_code)]
pub(crate) fn runtime_thread_history_migrator() -> Migrator {
    runtime_migrator(&THREAD_HISTORY_MIGRATOR)
}

pub(crate) async fn repair_legacy_state_migration_versions(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let migrations_table_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?
    .is_some();
    if !migrations_table_exists {
        return Ok(());
    }

    if let Some(recency_migration) = migrator
        .migrations
        .iter()
        .find(|migration| migration.version == 39)
    {
        let legacy_recency_needs_repair = sqlx::query_scalar::<_, i64>(
            r#"
SELECT 1
FROM _sqlx_migrations
WHERE version = ?
  AND checksum = ?
  AND NOT EXISTS (
      SELECT 1 FROM _sqlx_migrations WHERE version = ?
  )
            "#,
        )
        .bind(38_i64)
        .bind(recency_migration.checksum.as_ref())
        .bind(recency_migration.version)
        .fetch_optional(pool)
        .await?
        .is_some();
        if legacy_recency_needs_repair {
            sqlx::query(
                r#"
UPDATE _sqlx_migrations
SET version = ?, description = ?
WHERE version = ?
  AND checksum = ?
  AND NOT EXISTS (
      SELECT 1 FROM _sqlx_migrations WHERE version = ?
  )
                "#,
            )
            .bind(recency_migration.version)
            .bind(recency_migration.description.as_ref())
            .bind(38_i64)
            .bind(recency_migration.checksum.as_ref())
            .bind(recency_migration.version)
            .execute(pool)
            .await?;
        }
    }

    // Upstream assigned its pin and provider migrations versions 43 and 44, while this fork
    // already used version 43. Match the SQL checksums before shifting only those official
    // history entries forward to the versions used by the combined migration sequence.
    let Some(pin_migration) = migrator
        .migrations
        .iter()
        .find(|migration| migration.version == 44)
    else {
        return Ok(());
    };
    let Some(provider_migration) = migrator
        .migrations
        .iter()
        .find(|migration| migration.version == 45)
    else {
        return Ok(());
    };
    let version_43_checksum =
        sqlx::query_scalar::<_, Vec<u8>>("SELECT checksum FROM _sqlx_migrations WHERE version = ?")
            .bind(43_i64)
            .fetch_optional(pool)
            .await?;
    if version_43_checksum.as_deref() != Some(pin_migration.checksum.as_ref()) {
        return Ok(());
    }

    let version_44_checksum =
        sqlx::query_scalar::<_, Vec<u8>>("SELECT checksum FROM _sqlx_migrations WHERE version = ?")
            .bind(44_i64)
            .fetch_optional(pool)
            .await?;
    let provider_is_at_version_44 =
        version_44_checksum.as_deref() == Some(provider_migration.checksum.as_ref());
    if version_44_checksum.is_some() && !provider_is_at_version_44 {
        return Ok(());
    }
    if provider_is_at_version_44
        && sqlx::query_scalar::<_, i64>("SELECT 1 FROM _sqlx_migrations WHERE version = ?")
            .bind(45_i64)
            .fetch_optional(pool)
            .await?
            .is_some()
    {
        return Ok(());
    }

    let mut transaction = pool.begin().await?;
    if provider_is_at_version_44 {
        sqlx::query(
            "UPDATE _sqlx_migrations SET version = ?, description = ? WHERE version = ? AND checksum = ?",
        )
        .bind(provider_migration.version)
        .bind(provider_migration.description.as_ref())
        .bind(44_i64)
        .bind(provider_migration.checksum.as_ref())
        .execute(&mut *transaction)
        .await?;
    }
    sqlx::query(
        "UPDATE _sqlx_migrations SET version = ?, description = ? WHERE version = ? AND checksum = ?",
    )
    .bind(pin_migration.version)
    .bind(pin_migration.description.as_ref())
    .bind(43_i64)
    .bind(pin_migration.checksum.as_ref())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
pub(crate) mod tests;
