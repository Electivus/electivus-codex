use chrono::Utc;
use sqlx::AssertSqlSafe;
use sqlx::Postgres;

use super::PostgresThreadStore;
use super::database_error;
use crate::MoveThreadToSectionParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

const SECTION_POSITION_GAP: i64 = 1_000_000;

pub(super) async fn move_thread_to_section(
    store: &PostgresThreadStore,
    params: MoveThreadToSectionParams,
) -> ThreadStoreResult<()> {
    if params
        .section
        .as_deref()
        .is_some_and(|section| section.trim().is_empty())
    {
        return invalid_request("section must not be empty");
    }
    if params.section.is_none() && params.before_thread_id.is_some() {
        return invalid_request("before thread cannot be specified without a section");
    }

    if let Some(state_db) = store.state_db.as_ref() {
        let updated = state_db
            .move_thread_to_section(
                params.thread_id,
                params.section.as_deref(),
                params.before_thread_id,
            )
            .await
            .map_err(|error| map_state_error(error.to_string()))?;
        return if updated {
            Ok(())
        } else {
            Err(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            })
        };
    }

    let thread_id = params.thread_id.to_string();
    let mut transaction = store
        .pool
        .begin()
        .await
        .map_err(|error| database_error("begin thread section move", error))?;
    let current_section = sqlx::query_scalar::<_, Option<String>>(AssertSqlSafe(format!(
        "SELECT thread_section_id FROM {} WHERE thread_id = $1 FOR UPDATE",
        store.tables.threads
    )))
    .bind(&thread_id)
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error("lock thread for section move", error))?
    .ok_or(ThreadStoreError::ThreadNotFound {
        thread_id: params.thread_id,
    })?;
    let Some(section) = params.section.as_deref() else {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET thread_section_id = NULL, section_position = NULL, \
             section_entered_at = NULL, projection = jsonb_set(jsonb_set(jsonb_set(\
             projection, '{{section}}', 'null'::jsonb, TRUE), '{{section_position}}', \
             'null'::jsonb, TRUE), '{{section_entered_at}}', 'null'::jsonb, TRUE) \
             WHERE thread_id = $1",
            store.tables.threads
        )))
        .bind(&thread_id)
        .execute(transaction.as_mut())
        .await
        .map_err(|error| database_error("clear thread section", error))?;
        transaction
            .commit()
            .await
            .map_err(|error| database_error("commit thread section move", error))?;
        return Ok(());
    };

    let section_name = sqlx::query_scalar::<_, String>(AssertSqlSafe(format!(
        "SELECT name FROM {} WHERE id = $1",
        store.tables.sections
    )))
    .bind(section)
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| database_error("read destination thread section", error))?
    .ok_or_else(|| ThreadStoreError::InvalidRequest {
        message: format!("section {section} does not exist"),
    })?;
    let before_thread_id = params.before_thread_id.map(|id| id.to_string());
    if before_thread_id.as_deref() == Some(thread_id.as_str()) {
        return invalid_request(format!("thread {thread_id} cannot be moved before itself"));
    }
    if let Some(before_thread_id) = before_thread_id.as_deref() {
        let before_section = sqlx::query_scalar::<_, Option<String>>(AssertSqlSafe(format!(
            "SELECT thread_section_id FROM {} WHERE thread_id = $1 FOR UPDATE",
            store.tables.threads
        )))
        .bind(before_thread_id)
        .fetch_optional(transaction.as_mut())
        .await
        .map_err(|error| database_error("lock thread section move anchor", error))?;
        if before_section.flatten().as_deref() != Some(section) {
            return invalid_request(format!(
                "before thread {before_thread_id} is not in section {section}"
            ));
        }
    }

    let position = section_move_position(
        store,
        &mut transaction,
        section,
        &thread_id,
        before_thread_id.as_deref(),
    )
    .await?;
    if current_section.as_deref() == Some(section) {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET section_position = $1, projection = \
             jsonb_set(projection, '{{section_position}}', to_jsonb($1::bigint), TRUE) \
             WHERE thread_id = $2",
            store.tables.threads
        )))
        .bind(position)
        .bind(&thread_id)
        .execute(transaction.as_mut())
        .await
        .map_err(|error| database_error("reorder thread in section", error))?;
    } else {
        let entered_at = Utc::now();
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET thread_section_id = $1, section_position = $2, \
             section_entered_at = $3, projection = jsonb_set(jsonb_set(jsonb_set(\
             projection, '{{section}}', jsonb_build_object('id', $1::text, 'name', $4::text), \
             TRUE), '{{section_position}}', to_jsonb($2::bigint), TRUE), \
             '{{section_entered_at}}', to_jsonb($3::timestamptz), TRUE) WHERE thread_id = $5",
            store.tables.threads
        )))
        .bind(section)
        .bind(position)
        .bind(entered_at)
        .bind(section_name)
        .bind(&thread_id)
        .execute(transaction.as_mut())
        .await
        .map_err(|error| database_error("move thread into section", error))?;
    }
    transaction
        .commit()
        .await
        .map_err(|error| database_error("commit thread section move", error))?;
    Ok(())
}

async fn section_move_position(
    store: &PostgresThreadStore,
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    section: &str,
    thread_id: &str,
    before_thread_id: Option<&str>,
) -> ThreadStoreResult<i64> {
    let mut renumbered = false;
    loop {
        let position = if let Some(before_thread_id) = before_thread_id {
            let upper = sqlx::query_scalar::<_, Option<i64>>(AssertSqlSafe(format!(
                "SELECT section_position FROM {} WHERE thread_id = $1 AND thread_section_id = $2",
                store.tables.threads
            )))
            .bind(before_thread_id)
            .bind(section)
            .fetch_optional(transaction.as_mut())
            .await
            .map_err(|error| database_error("read thread section move anchor", error))?
            .flatten()
            .ok_or_else(|| ThreadStoreError::InvalidRequest {
                message: format!("before thread {before_thread_id} is not in section {section}"),
            })?;
            let lower = sqlx::query_scalar::<_, Option<i64>>(AssertSqlSafe(format!(
                "SELECT MAX(section_position) FROM {} WHERE thread_section_id = $1 \
                 AND section_position < $2 AND thread_id <> $3",
                store.tables.threads
            )))
            .bind(section)
            .bind(upper)
            .bind(thread_id)
            .fetch_one(transaction.as_mut())
            .await
            .map_err(|error| database_error("read preceding section position", error))?;
            match lower {
                Some(lower) if i128::from(upper) - i128::from(lower) > 1 => Some(
                    i64::try_from(i128::from(lower) + (i128::from(upper) - i128::from(lower)) / 2)
                        .map_err(|_| ThreadStoreError::Internal {
                            message: "thread section position overflowed".to_string(),
                        })?,
                ),
                Some(_) => None,
                None if upper > 1 => Some(upper / 2),
                None => None,
            }
        } else {
            let max_position = sqlx::query_scalar::<_, Option<i64>>(AssertSqlSafe(format!(
                "SELECT MAX(section_position) FROM {} WHERE thread_section_id = $1 \
                 AND thread_id <> $2",
                store.tables.threads
            )))
            .bind(section)
            .bind(thread_id)
            .fetch_one(transaction.as_mut())
            .await
            .map_err(|error| database_error("read final section position", error))?;
            max_position
                .unwrap_or_default()
                .checked_add(SECTION_POSITION_GAP)
        };
        if let Some(position) = position {
            return Ok(position);
        }
        if renumbered {
            return Err(ThreadStoreError::Internal {
                message: format!("section {section} has no remaining thread positions"),
            });
        }
        renumber_section_positions(store, transaction, section, Some(thread_id)).await?;
        renumbered = true;
    }
}

async fn renumber_section_positions(
    store: &PostgresThreadStore,
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    section: &str,
    excluded_thread_id: Option<&str>,
) -> ThreadStoreResult<()> {
    sqlx::query(AssertSqlSafe(format!(
        "WITH ranked AS (SELECT thread_id, ROW_NUMBER() OVER (ORDER BY section_position ASC, \
         thread_id ASC) * $1 AS position FROM {} WHERE thread_section_id = $2 \
         AND ($3::text IS NULL OR thread_id <> $3)) UPDATE {} AS target \
         SET section_position = ranked.position, projection = jsonb_set(target.projection, \
         '{{section_position}}', to_jsonb(ranked.position), TRUE) FROM ranked \
         WHERE target.thread_id = ranked.thread_id",
        store.tables.threads, store.tables.threads
    )))
    .bind(SECTION_POSITION_GAP)
    .bind(section)
    .bind(excluded_thread_id)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| database_error("renumber thread section positions", error))?;
    Ok(())
}

fn invalid_request<T>(message: impl Into<String>) -> ThreadStoreResult<T> {
    Err(ThreadStoreError::InvalidRequest {
        message: message.into(),
    })
}

fn map_state_error(message: String) -> ThreadStoreError {
    if message.starts_with("before thread ")
        || message.starts_with("thread ")
        || message.starts_with("section ")
    {
        ThreadStoreError::InvalidRequest { message }
    } else {
        ThreadStoreError::Internal {
            message: format!("failed to move thread section: {message}"),
        }
    }
}
