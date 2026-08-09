use crate::ThreadSection;
use crate::postgres::map_sql_error;
use crate::postgres::qualified_table;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;

pub(super) async fn create_thread_section(
    pool: &PgPool,
    schema: &str,
    section: ThreadSection,
) -> anyhow::Result<ThreadSection> {
    let sections = qualified_table(schema, "thread_sections");
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {sections} (id, name) VALUES ($1, $2)"
    )))
    .bind(&section.id)
    .bind(&section.name)
    .execute(pool)
    .await
    .map_err(|error| map_sql_error(schema, "create thread section", error))?;
    Ok(section)
}

pub(super) async fn rename_thread_section(
    pool: &PgPool,
    schema: &str,
    id: &str,
    name: &str,
) -> anyhow::Result<Option<ThreadSection>> {
    let sections = qualified_table(schema, "thread_sections");
    let threads = qualified_table(schema, "threads");
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin thread section rename", error))?;
    let section = sqlx::query_as::<_, (String, String)>(AssertSqlSafe(format!(
        "UPDATE {sections} SET name = $1 WHERE id = $2 RETURNING id, name"
    )))
    .bind(name)
    .bind(id)
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "rename thread section", error))?;
    if section.is_some() {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {threads} SET projection = jsonb_set(projection, '{{section,name}}', \
             to_jsonb($1::text), TRUE) WHERE thread_section_id = $2"
        )))
        .bind(name)
        .bind(id)
        .execute(transaction.as_mut())
        .await
        .map_err(|error| map_sql_error(schema, "refresh renamed thread section", error))?;
    }
    transaction
        .commit()
        .await
        .map_err(|error| map_sql_error(schema, "commit thread section rename", error))?;
    Ok(section.map(|(id, name)| ThreadSection { id, name }))
}

pub(super) async fn delete_thread_section(
    pool: &PgPool,
    schema: &str,
    id: &str,
) -> anyhow::Result<bool> {
    let sections = qualified_table(schema, "thread_sections");
    let threads = qualified_table(schema, "threads");
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin thread section deletion", error))?;
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {threads} SET thread_section_id = NULL, section_position = NULL, \
         section_entered_at = NULL, projection = jsonb_set(jsonb_set(jsonb_set(\
         projection, '{{section}}', 'null'::jsonb, TRUE), '{{section_position}}', \
         'null'::jsonb, TRUE), '{{section_entered_at}}', 'null'::jsonb, TRUE) \
         WHERE thread_section_id = $1"
    )))
    .bind(id)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "clear deleted thread section", error))?;
    let deleted = sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {sections} WHERE id = $1"
    )))
    .bind(id)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "delete thread section", error))?
    .rows_affected()
        > 0;
    transaction
        .commit()
        .await
        .map_err(|error| map_sql_error(schema, "commit thread section deletion", error))?;
    Ok(deleted)
}
