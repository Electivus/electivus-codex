use crate::ThreadSection;
use crate::ThreadSectionAppearance;
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
    let appearance = section
        .appearance
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {sections} (id, name, appearance) VALUES ($1, $2, $3)"
    )))
    .bind(&section.id)
    .bind(&section.name)
    .bind(appearance)
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
    appearance: Option<Option<ThreadSectionAppearance>>,
) -> anyhow::Result<Option<ThreadSection>> {
    let sections = qualified_table(schema, "thread_sections");
    let threads = qualified_table(schema, "threads");
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin thread section rename", error))?;
    let replace_appearance = appearance.is_some();
    let appearance = appearance
        .flatten()
        .map(|appearance| serde_json::to_string(&appearance))
        .transpose()?;
    let section = sqlx::query_as::<_, (String, String, Option<String>)>(AssertSqlSafe(format!(
        "UPDATE {sections} SET name = $1, appearance = CASE WHEN $2 THEN $3 ELSE appearance END \
         WHERE id = $4 RETURNING id, name, appearance"
    )))
    .bind(name)
    .bind(replace_appearance)
    .bind(appearance)
    .bind(id)
    .fetch_optional(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "rename thread section", error))?;
    let section = section.map(ThreadSection::from_row).transpose()?;
    if let Some(section) = section.as_ref() {
        let section = serde_json::to_value(section)?;
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {threads} SET projection = jsonb_set(projection, '{{section}}', $1, TRUE) \
             WHERE thread_section_id = $2"
        )))
        .bind(section)
        .bind(id)
        .execute(transaction.as_mut())
        .await
        .map_err(|error| map_sql_error(schema, "refresh renamed thread section", error))?;
    }
    transaction
        .commit()
        .await
        .map_err(|error| map_sql_error(schema, "commit thread section rename", error))?;
    Ok(section)
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
