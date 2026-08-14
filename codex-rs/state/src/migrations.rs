use std::borrow::Cow;

use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

pub(crate) static STATE_MIGRATOR: Migrator = sqlx::migrate!("./migrations");
pub(crate) static LOGS_MIGRATOR: Migrator = sqlx::migrate!("./logs_migrations");
pub(crate) static GOALS_MIGRATOR: Migrator = sqlx::migrate!("./goals_migrations");
pub(crate) static MEMORIES_MIGRATOR: Migrator = sqlx::migrate!("./memory_migrations");
pub(crate) static QUEUE_MIGRATOR: Migrator = sqlx::migrate!("./queue_migrations");
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

pub(crate) fn runtime_queue_migrator() -> Migrator {
    runtime_migrator(&QUEUE_MIGRATOR)
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

    // Upstream used versions 43-48 for pin, provider, section, section-order, rollout-state,
    // and section-appearance migrations. The combined sequence inserts fork migrations into
    // that range. Match applied migrations by checksum and move the entire known suffix through
    // temporary versions before assigning the combined versions so crossing remaps cannot collide.
    let applied = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version >= 43 ORDER BY version",
    )
    .fetch_all(pool)
    .await?;
    let mut remaps = Vec::new();
    for (applied_version, checksum) in &applied {
        let Some(migration) = migrator
            .migrations
            .iter()
            .find(|migration| migration.checksum.as_ref() == checksum.as_slice())
        else {
            continue;
        };
        if migration.version != *applied_version {
            remaps.push((*applied_version, migration));
        }
    }
    if remaps.is_empty() {
        return Ok(());
    }

    let remap_sources = remaps
        .iter()
        .map(|(version, _)| *version)
        .collect::<std::collections::HashSet<_>>();
    for (_, migration) in &remaps {
        if let Some((_, checksum)) = applied
            .iter()
            .find(|(version, _)| *version == migration.version)
            && checksum.as_slice() != migration.checksum.as_ref()
            && !remap_sources.contains(&migration.version)
        {
            return Ok(());
        }
    }

    let mut transaction = pool.begin().await?;
    let temporary_remaps = remaps
        .iter()
        .enumerate()
        .map(|(index, (legacy_version, migration))| {
            (i64::MIN + index as i64, *legacy_version, *migration)
        })
        .collect::<Vec<_>>();
    for (temporary_version, legacy_version, migration) in &temporary_remaps {
        sqlx::query("UPDATE _sqlx_migrations SET version = ? WHERE version = ? AND checksum = ?")
            .bind(temporary_version)
            .bind(legacy_version)
            .bind(migration.checksum.as_ref())
            .execute(&mut *transaction)
            .await?;
    }
    for (temporary_version, _, migration) in temporary_remaps {
        sqlx::query(
            "UPDATE _sqlx_migrations SET version = ?, description = ? WHERE version = ? AND checksum = ?",
        )
        .bind(migration.version)
        .bind(migration.description.as_ref())
        .bind(temporary_version)
        .bind(migration.checksum.as_ref())
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
pub(crate) mod tests;
