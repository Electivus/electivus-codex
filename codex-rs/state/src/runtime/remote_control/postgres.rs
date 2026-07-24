use super::RemoteControlEnrollmentRecord;
use super::app_server_client_name_from_key;
use super::app_server_client_name_key;
use crate::postgres::qualified_table;
use chrono::Utc;
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use sqlx::Row;

#[derive(Clone)]
pub(super) struct PostgresRemoteControlEnrollmentStore {
    pool: PgPool,
    table: String,
}

impl PostgresRemoteControlEnrollmentStore {
    pub(super) fn new(pool: PgPool, schema: String) -> Self {
        let table = qualified_table(&schema, "remote_control_enrollments");
        Self { pool, table }
    }

    pub(super) async fn get(
        &self,
        websocket_url: &str,
        account_id: &str,
        app_server_client_name: Option<&str>,
    ) -> anyhow::Result<Option<RemoteControlEnrollmentRecord>> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT websocket_url, account_id, app_server_client_name, server_id, \
             environment_id, server_name, remote_control_enabled FROM {} \
             WHERE websocket_url = $1 AND account_id = $2 AND app_server_client_name = $3",
            self.table
        )))
        .bind(websocket_url)
        .bind(account_id)
        .bind(app_server_client_name_key(app_server_client_name))
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            let app_server_client_name: String = row.try_get("app_server_client_name")?;
            Ok(RemoteControlEnrollmentRecord {
                websocket_url: row.try_get("websocket_url")?,
                account_id: row.try_get("account_id")?,
                app_server_client_name: app_server_client_name_from_key(app_server_client_name),
                server_id: row.try_get("server_id")?,
                environment_id: row.try_get("environment_id")?,
                server_name: row.try_get("server_name")?,
                remote_control_enabled: row.try_get("remote_control_enabled")?,
            })
        })
        .transpose()
    }

    pub(super) async fn upsert(
        &self,
        enrollment: &RemoteControlEnrollmentRecord,
    ) -> anyhow::Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (websocket_url, account_id, app_server_client_name, server_id, \
             environment_id, server_name, remote_control_enabled, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT(websocket_url, account_id, app_server_client_name) DO UPDATE SET \
             server_id = excluded.server_id, environment_id = excluded.environment_id, \
             server_name = excluded.server_name, updated_at = excluded.updated_at",
            self.table
        )))
        .bind(&enrollment.websocket_url)
        .bind(&enrollment.account_id)
        .bind(app_server_client_name_key(
            enrollment.app_server_client_name.as_deref(),
        ))
        .bind(&enrollment.server_id)
        .bind(&enrollment.environment_id)
        .bind(&enrollment.server_name)
        .bind(enrollment.remote_control_enabled)
        .bind(Utc::now().timestamp())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(super) async fn set_enabled(
        &self,
        websocket_url: &str,
        account_id: &str,
        app_server_client_name: Option<&str>,
        remote_control_enabled: bool,
    ) -> anyhow::Result<u64> {
        let result = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET remote_control_enabled = $1, updated_at = $2 \
             WHERE websocket_url = $3 AND account_id = $4 AND app_server_client_name = $5",
            self.table
        )))
        .bind(remote_control_enabled)
        .bind(Utc::now().timestamp())
        .bind(websocket_url)
        .bind(account_id)
        .bind(app_server_client_name_key(app_server_client_name))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub(super) async fn delete(
        &self,
        websocket_url: &str,
        account_id: &str,
        app_server_client_name: Option<&str>,
    ) -> anyhow::Result<u64> {
        let result = sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM {} WHERE websocket_url = $1 AND account_id = $2 \
             AND app_server_client_name = $3",
            self.table
        )))
        .bind(websocket_url)
        .bind(account_id)
        .bind(app_server_client_name_key(app_server_client_name))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    #[cfg(test)]
    pub(super) async fn close(&self) {
        self.pool.close().await;
    }
}
