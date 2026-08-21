use crate::ThreadSection;
use crate::ThreadSectionsPage;
use crate::postgres::map_sql_error;
use crate::postgres::qualified_table;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::QueryBuilder;
use sqlx::Row;
use std::collections::HashMap;

use super::SECTION_POSITION_GAP;

pub(super) async fn get_thread_section_ordering(
    pool: &PgPool,
    schema: &str,
    thread_ids: &[ThreadId],
) -> anyhow::Result<HashMap<ThreadId, (Option<i64>, Option<DateTime<Utc>>)>> {
    let threads = qualified_table(schema, "threads");
    let thread_ids = thread_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let rows = sqlx::query(AssertSqlSafe(format!(
        "SELECT thread_id, section_position, section_entered_at FROM {threads} \
         WHERE thread_id = ANY($1)"
    )))
    .bind(&thread_ids)
    .fetch_all(pool)
    .await
    .map_err(|error| map_sql_error(schema, "read thread section ordering", error))?;
    rows.into_iter()
        .map(|row| {
            let thread_id: String = row
                .try_get("thread_id")
                .map_err(|error| map_sql_error(schema, "decode thread section ordering", error))?;
            let position = row
                .try_get("section_position")
                .map_err(|error| map_sql_error(schema, "decode thread section ordering", error))?;
            let entered_at = row
                .try_get("section_entered_at")
                .map_err(|error| map_sql_error(schema, "decode thread section ordering", error))?;
            Ok((ThreadId::try_from(thread_id)?, (position, entered_at)))
        })
        .collect()
}

pub(super) async fn get_thread_section(
    pool: &PgPool,
    schema: &str,
    id: &str,
) -> anyhow::Result<Option<ThreadSection>> {
    let sections = qualified_table(schema, "thread_sections");
    let row = sqlx::query_as::<_, (String, String, Option<String>)>(AssertSqlSafe(format!(
        "SELECT id, name, appearance FROM {sections} WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| map_sql_error(schema, "read thread section", error))?;
    row.map(ThreadSection::from_row).transpose()
}

pub(super) async fn list_thread_sections(
    pool: &PgPool,
    schema: &str,
    cursor: Option<&str>,
    limit: usize,
) -> anyhow::Result<ThreadSectionsPage> {
    let sections = qualified_table(schema, "thread_sections");
    let page_size = limit.max(1);
    let fetch_limit = i64::try_from(page_size.saturating_add(1))?;
    let mut query = QueryBuilder::<Postgres>::new(format!(
        "SELECT id, name, appearance FROM {sections} WHERE 1 = 1"
    ));
    if let Some(cursor) = cursor {
        query.push(" AND id > ");
        query.push_bind(cursor);
    }
    query.push(" ORDER BY id LIMIT ");
    query.push_bind(fetch_limit);
    let rows = query
        .build_query_as::<(String, String, Option<String>)>()
        .fetch_all(pool)
        .await
        .map_err(|error| map_sql_error(schema, "list thread sections", error))?;
    let mut sections = rows
        .into_iter()
        .map(ThreadSection::from_row)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let next_cursor = if sections.len() > page_size {
        sections.pop();
        sections.last().map(|section| section.id.clone())
    } else {
        None
    };
    Ok(ThreadSectionsPage {
        sections,
        next_cursor,
    })
}

pub(super) async fn move_thread_to_section(
    pool: &PgPool,
    schema: &str,
    thread_id: ThreadId,
    section: Option<&str>,
    before_thread_id: Option<ThreadId>,
) -> anyhow::Result<bool> {
    let threads = qualified_table(schema, "threads");
    let sections = qualified_table(schema, "thread_sections");
    let thread_id = thread_id.to_string();
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin thread section move", error))?;
    let current_section = sqlx::query_scalar::<_, Option<String>>(AssertSqlSafe(format!(
        "SELECT thread_section_id FROM {threads} WHERE thread_id = $1 FOR UPDATE"
    )))
    .bind(&thread_id)
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "lock thread for section move", error))?;
    let Some(current_section) = current_section else {
        return Ok(false);
    };
    let Some(section) = section else {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {threads} SET thread_section_id = NULL, section_position = NULL, \
             section_entered_at = NULL, projection = jsonb_set(jsonb_set(jsonb_set(\
             projection, '{{section}}', 'null'::jsonb, TRUE), '{{section_position}}', \
             'null'::jsonb, TRUE), '{{section_entered_at}}', 'null'::jsonb, TRUE) \
             WHERE thread_id = $1"
        )))
        .bind(&thread_id)
        .execute(transaction.as_mut())
        .await
        .map_err(|error| map_sql_error(schema, "clear thread section", error))?;
        transaction
            .commit()
            .await
            .map_err(|error| map_sql_error(schema, "commit thread section move", error))?;
        return Ok(true);
    };

    let section_row = sqlx::query_as::<_, (String, String, Option<String>)>(AssertSqlSafe(
        format!("SELECT id, name, appearance FROM {sections} WHERE id = $1"),
    ))
    .bind(section)
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "read destination thread section", error))?
    .map(ThreadSection::from_row)
    .transpose()?
    .ok_or_else(|| anyhow::anyhow!("section {section} does not exist"))?;
    let section_projection = serde_json::to_value(section_row)?;

    let before_thread_id = before_thread_id.map(|id| id.to_string());
    if before_thread_id.as_deref() == Some(thread_id.as_str()) {
        anyhow::bail!("thread {thread_id} cannot be moved before itself");
    }
    if let Some(before_thread_id) = before_thread_id.as_deref() {
        let before_section = sqlx::query_scalar::<_, Option<String>>(AssertSqlSafe(format!(
            "SELECT thread_section_id FROM {threads} WHERE thread_id = $1 FOR UPDATE"
        )))
        .bind(before_thread_id)
        .fetch_optional(transaction.as_mut())
        .await
        .map_err(|error| map_sql_error(schema, "lock thread section move anchor", error))?;
        if before_section.flatten().as_deref() != Some(section) {
            anyhow::bail!("before thread {before_thread_id} is not in section {section}");
        }
    }

    let position = section_move_position(
        &mut transaction,
        schema,
        &threads,
        section,
        &thread_id,
        before_thread_id.as_deref(),
    )
    .await?;
    if current_section.as_deref() == Some(section) {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {threads} SET section_position = $1, projection = \
             jsonb_set(projection, '{{section_position}}', to_jsonb($1::bigint), TRUE) \
             WHERE thread_id = $2"
        )))
        .bind(position)
        .bind(&thread_id)
        .execute(transaction.as_mut())
        .await
        .map_err(|error| map_sql_error(schema, "reorder thread in section", error))?;
    } else {
        let entered_at = Utc::now();
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {threads} SET thread_section_id = $1, section_position = $2, \
             section_entered_at = $3, projection = jsonb_set(jsonb_set(jsonb_set(\
             projection, '{{section}}', $4, \
             TRUE), '{{section_position}}', to_jsonb($2::bigint), TRUE), \
             '{{section_entered_at}}', to_jsonb($3::timestamptz), TRUE) WHERE thread_id = $5"
        )))
        .bind(section)
        .bind(position)
        .bind(entered_at)
        .bind(section_projection)
        .bind(&thread_id)
        .execute(transaction.as_mut())
        .await
        .map_err(|error| map_sql_error(schema, "move thread into section", error))?;
    }
    transaction
        .commit()
        .await
        .map_err(|error| map_sql_error(schema, "commit thread section move", error))?;
    Ok(true)
}

async fn section_move_position(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    schema: &str,
    threads: &str,
    section: &str,
    thread_id: &str,
    before_thread_id: Option<&str>,
) -> anyhow::Result<i64> {
    let mut renumbered = false;
    loop {
        let position = if let Some(before_thread_id) = before_thread_id {
            let upper = sqlx::query_scalar::<_, Option<i64>>(AssertSqlSafe(format!(
                "SELECT section_position FROM {threads} WHERE thread_id = $1 \
                 AND thread_section_id = $2"
            )))
            .bind(before_thread_id)
            .bind(section)
            .fetch_optional(transaction.as_mut())
            .await
            .map_err(|error| map_sql_error(schema, "read thread section move anchor", error))?
            .flatten()
            .ok_or_else(|| {
                anyhow::anyhow!("before thread {before_thread_id} is not in section {section}")
            })?;
            let lower = sqlx::query_scalar::<_, Option<i64>>(AssertSqlSafe(format!(
                "SELECT MAX(section_position) FROM {threads} WHERE thread_section_id = $1 \
                 AND section_position < $2 AND thread_id <> $3"
            )))
            .bind(section)
            .bind(upper)
            .bind(thread_id)
            .fetch_one(transaction.as_mut())
            .await
            .map_err(|error| map_sql_error(schema, "read preceding section position", error))?;
            match lower {
                Some(lower) if i128::from(upper) - i128::from(lower) > 1 => Some(i64::try_from(
                    i128::from(lower) + (i128::from(upper) - i128::from(lower)) / 2,
                )?),
                Some(_) => None,
                None if upper > 1 => Some(upper / 2),
                None => None,
            }
        } else {
            let max_position = sqlx::query_scalar::<_, Option<i64>>(AssertSqlSafe(format!(
                "SELECT MAX(section_position) FROM {threads} WHERE thread_section_id = $1 \
                 AND thread_id <> $2"
            )))
            .bind(section)
            .bind(thread_id)
            .fetch_one(transaction.as_mut())
            .await
            .map_err(|error| map_sql_error(schema, "read final section position", error))?;
            max_position
                .unwrap_or_default()
                .checked_add(SECTION_POSITION_GAP)
        };
        if let Some(position) = position {
            return Ok(position);
        }
        if renumbered {
            anyhow::bail!("section {section} has no remaining thread positions");
        }
        renumber_section_positions(transaction, schema, threads, section, Some(thread_id)).await?;
        renumbered = true;
    }
}

async fn renumber_section_positions(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    schema: &str,
    threads: &str,
    section: &str,
    excluded_thread_id: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(AssertSqlSafe(format!(
        "WITH ranked AS (SELECT thread_id, ROW_NUMBER() OVER (ORDER BY section_position ASC, \
         thread_id ASC) * $1 AS position FROM {threads} WHERE thread_section_id = $2 \
         AND ($3::text IS NULL OR thread_id <> $3)) UPDATE {threads} AS target \
         SET section_position = ranked.position, projection = jsonb_set(target.projection, \
         '{{section_position}}', to_jsonb(ranked.position), TRUE) FROM ranked \
         WHERE target.thread_id = ranked.thread_id"
    )))
    .bind(SECTION_POSITION_GAP)
    .bind(section)
    .bind(excluded_thread_id)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "renumber thread section positions", error))?;
    Ok(())
}
