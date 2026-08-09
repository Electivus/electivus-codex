use super::StateRuntime;
use crate::PINNED_THREAD_SECTION_ID;
use crate::ThreadSection;
use uuid::Uuid;

mod postgres;

impl StateRuntime {
    /// Create a custom thread section with a stable, server-assigned UUIDv7.
    pub async fn create_thread_section(&self, name: &str) -> anyhow::Result<ThreadSection> {
        let section = ThreadSection {
            id: Uuid::now_v7().to_string(),
            name: name.to_string(),
        };

        if let Some((pool, schema)) = self.postgres_connection() {
            return postgres::create_thread_section(&pool, &schema, section).await;
        }

        sqlx::query("INSERT INTO thread_sections (id, name) VALUES (?, ?)")
            .bind(&section.id)
            .bind(&section.name)
            .execute(self.sqlite_pool()?)
            .await?;

        Ok(section)
    }

    /// Rename a custom thread section without changing its stable identity.
    pub async fn rename_thread_section(
        &self,
        id: &str,
        name: &str,
    ) -> anyhow::Result<Option<ThreadSection>> {
        if id == PINNED_THREAD_SECTION_ID {
            anyhow::bail!("built-in pinned thread section cannot be renamed");
        }

        if let Some((pool, schema)) = self.postgres_connection() {
            return postgres::rename_thread_section(&pool, &schema, id, name).await;
        }

        let section = sqlx::query_as::<_, (String, String)>(
            "UPDATE thread_sections SET name = ? WHERE id = ? RETURNING id, name",
        )
        .bind(name)
        .bind(id)
        .fetch_optional(self.sqlite_pool()?)
        .await?;

        Ok(section.map(|(id, name)| ThreadSection { id, name }))
    }

    /// Delete a custom section and return its threads to the unsectioned list.
    pub async fn delete_thread_section(&self, id: &str) -> anyhow::Result<bool> {
        if id == PINNED_THREAD_SECTION_ID {
            anyhow::bail!("built-in pinned thread section cannot be deleted");
        }

        if let Some((pool, schema)) = self.postgres_connection() {
            return postgres::delete_thread_section(&pool, &schema, id).await;
        }

        let mut tx = self.sqlite_pool()?.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "UPDATE threads SET section_position = NULL, section_entered_at_ms = NULL WHERE thread_section_id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;

        let deleted = sqlx::query("DELETE FROM thread_sections WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        tx.commit().await?;

        Ok(deleted)
    }
}

#[cfg(test)]
#[path = "thread_sections_tests.rs"]
mod tests;
