use super::progress::inspect_existing_progress;
use crate::PostgresNamespaceAction;
use crate::PostgresNamespaceConfig;
use crate::postgres::MAXIMUM_COMPATIBLE_SCHEMA_VERSION;
use crate::postgres::config::connect_pool;
use crate::postgres::config::connection_failed;
use crate::postgres::manage_postgres_namespace_with_connection;
use crate::postgres::map_sql_error;
use crate::postgres::qualified_table;
use sqlx::AssertSqlSafe;
use sqlx::Connection;

const EXPECTED_SCHEMA_FINGERPRINT: &str = "6335d742dddaad2ad7c136ce329bf398";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DestinationState {
    pub(super) schema: String,
    pub(super) version: i64,
}

pub(super) async fn inspect(config: &PostgresNamespaceConfig) -> anyhow::Result<DestinationState> {
    inspect_with_source(config, /*expected_source*/ None).await
}

pub(super) async fn inspect_resumable(
    config: &PostgresNamespaceConfig,
    source_identity: &str,
    source_fingerprint: &str,
) -> anyhow::Result<DestinationState> {
    inspect_with_source(config, Some((source_identity, source_fingerprint))).await
}

async fn inspect_with_source(
    config: &PostgresNamespaceConfig,
    expected_source: Option<(&str, &str)>,
) -> anyhow::Result<DestinationState> {
    let pool = connect_pool(config).await?;
    let result = async {
        let mut connection = pool
            .acquire()
            .await
            .map_err(|_| connection_failed(config.url_env()))?;
        let mut transaction = connection
            .begin()
            .await
            .map_err(|error| map_sql_error(config.schema(), "begin migration preflight", error))?;
        sqlx::query("SET TRANSACTION READ ONLY")
            .execute(transaction.as_mut())
            .await
            .map_err(|error| {
                map_sql_error(config.schema(), "make migration preflight read-only", error)
            })?;
        let status = manage_postgres_namespace_with_connection(
            config,
            transaction.as_mut(),
            PostgresNamespaceAction::Validate,
        )
        .await?;
        anyhow::ensure!(
            status.version() == MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
            "PostgreSQL schema `{}` is at version {}, but Runtime State Migration requires the current version {}; run `codex state schema migrate` and retry",
            status.schema(),
            status.version(),
            MAXIMUM_COMPATIBLE_SCHEMA_VERSION
        );
        if let Err(empty_error) = ensure_empty(transaction.as_mut(), status.schema()).await {
            let Some((source_identity, source_fingerprint)) = expected_source else {
                return Err(empty_error);
            };
            match inspect_existing_progress(
                transaction.as_mut(),
                status.schema(),
                source_identity,
                source_fingerprint,
            )
            .await
            {
                Ok(Some(_)) => {}
                Ok(None) => return Err(empty_error),
                Err(error) => return Err(error),
            }
        }
        transaction.rollback().await.map_err(|error| {
            map_sql_error(config.schema(), "finish migration preflight", error)
        })?;
        Ok(DestinationState {
            schema: status.schema().to_string(),
            version: status.version(),
        })
    }
    .await;
    pool.close().await;
    result
}

pub(crate) async fn ensure_empty(
    connection: &mut sqlx::PgConnection,
    schema: &str,
) -> anyhow::Result<()> {
    validate_layout(connection, schema).await?;
    let backfill_state = qualified_table(schema, "backfill_state");
    let backfill_is_baseline: bool = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) = 1 AND BOOL_AND(\
         id = 1 AND status = 'pending' AND last_watermark IS NULL \
         AND last_success_at IS NULL AND owner_id IS NULL \
         AND fencing_token = 0 AND lease_expires_at IS NULL) \
         FROM {backfill_state}"
    )))
    .fetch_one(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "inspect destination backfill state", error))?;
    let generation_state = qualified_table(schema, "memory_generation_state");
    let generation_is_baseline: bool = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) = 1 AND BOOL_AND(singleton AND active_generation_id IS NULL) \
         FROM {generation_state}"
    )))
    .fetch_one(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "inspect destination memory state", error))?;
    anyhow::ensure!(
        backfill_is_baseline && generation_is_baseline,
        "PostgreSQL Runtime State Namespace `{schema}` contains non-baseline coordination state; provision an empty migrated namespace and retry"
    );
    let tables = sqlx::query_scalar::<_, String>(
        "SELECT c.relname FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') \
         ORDER BY c.relname",
    )
    .bind(schema)
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "inventory destination tables", error))?;
    let mut populated = Vec::new();
    for table in tables {
        if matches!(
            table.as_str(),
            "_codex_runtime_state_migrations" | "backfill_state" | "memory_generation_state"
        ) {
            continue;
        }
        let qualified = qualified_table(schema, &table);
        let row_count: i64 =
            sqlx::query_scalar(AssertSqlSafe(format!("SELECT COUNT(*) FROM {qualified}")))
                .fetch_one(&mut *connection)
                .await
                .map_err(|error| map_sql_error(schema, "count destination records", error))?;
        if row_count > 0 {
            populated.push(format!("{table} ({row_count})"));
        }
    }
    anyhow::ensure!(
        populated.is_empty(),
        "PostgreSQL Runtime State Namespace `{schema}` is not empty: {}; provision an empty migrated namespace and retry",
        populated.join(", ")
    );
    Ok(())
}

pub(crate) async fn validate_layout(
    connection: &mut sqlx::PgConnection,
    schema: &str,
) -> anyhow::Result<()> {
    let schema_fingerprint: String = sqlx::query_scalar(
        "WITH definitions AS (SELECT concat_ws('|', 'column', table_name, ordinal_position, column_name, data_type, udt_name, is_nullable, COALESCE(column_default, ''), is_identity, COALESCE(identity_generation, '')) AS definition FROM information_schema.columns WHERE table_schema = $1 UNION ALL SELECT concat_ws('|', 'constraint', c.relname, x.conname, x.contype, replace(pg_get_constraintdef(x.oid), quote_ident($1) || '.', '')) FROM pg_constraint x JOIN pg_class c ON c.oid = x.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 UNION ALL SELECT concat_ws('|', 'relation', c.relkind, c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 UNION ALL SELECT concat_ws('|', 'function', p.proname, replace(pg_get_functiondef(p.oid), quote_ident($1) || '.', '')) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = $1 UNION ALL SELECT concat_ws('|', 'trigger', c.relname, t.tgname, replace(pg_get_triggerdef(t.oid), quote_ident($1) || '.', '')) FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND NOT t.tgisinternal) SELECT md5(string_agg(definition, E'\\n' ORDER BY definition)) FROM definitions",
    )
    .bind(schema)
    .fetch_one(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "validate destination schema layout", error))?;
    anyhow::ensure!(
        schema_fingerprint == EXPECTED_SCHEMA_FINGERPRINT,
        "PostgreSQL Runtime State Namespace `{schema}` has an incompatible table layout; provision a freshly migrated namespace and retry"
    );
    Ok(())
}
