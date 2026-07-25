use super::RuntimeStateMigrationInventory;
use super::RuntimeStateMigrationPhase;
use super::RuntimeStateMigrationProgress;
use super::import_threads::fingerprint;
use super::import_threads::revalidate_source;
use super::import_threads::source_identity;
use super::progress::RuntimeStateMigrationEvidence;
use super::progress::existing_progress;
use super::progress::namespace_digest;
use super::progress::phase_evidence;
use super::snapshot_operational::OperationalSnapshot;
use super::snapshot_operational::snapshot_operational_state;
use crate::PostgresNamespaceAction;
use crate::PostgresNamespaceConfig;
use crate::SqliteConfig;
use crate::postgres::MAXIMUM_COMPATIBLE_SCHEMA_VERSION;
use crate::postgres::acquire_namespace_lock;
use crate::postgres::config::connect_pool;
use crate::postgres::manage_postgres_namespace_with_connection;
use crate::postgres::map_sql_error;
use crate::postgres::qualified_table;
use crate::postgres::quote_identifier;
use anyhow::Context;
use sqlx::AssertSqlSafe;

/// Revalidate and atomically import non-memory operational Runtime State into PostgreSQL.
pub async fn import_runtime_state_operational(
    source: &SqliteConfig,
    destination: &PostgresNamespaceConfig,
    expected_inventory: &RuntimeStateMigrationInventory,
) -> anyhow::Result<RuntimeStateMigrationProgress> {
    anyhow::ensure!(
        expected_inventory.destination_schema == destination.schema()
            && expected_inventory.destination_schema_version == MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
        "Runtime State Migration inventory does not match the current PostgreSQL destination"
    );
    revalidate_source(source, expected_inventory).await?;
    let snapshot = snapshot_operational_state(source).await?;
    revalidate_source(source, expected_inventory).await?;
    let source_identity = source_identity(source);
    let source_fingerprint = fingerprint(expected_inventory);
    let pool = connect_pool(destination).await?;
    revalidate_source(source, expected_inventory).await?;
    let result = import_snapshot(
        destination,
        &snapshot,
        &source_identity,
        &source_fingerprint,
        &pool,
    )
    .await;
    pool.close().await;
    result
}

async fn import_snapshot(
    destination: &PostgresNamespaceConfig,
    snapshot: &OperationalSnapshot,
    source_identity: &str,
    source_fingerprint: &str,
    pool: &sqlx::PgPool,
) -> anyhow::Result<RuntimeStateMigrationProgress> {
    let schema = destination.schema();
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| map_sql_error(schema, "begin operational migration", error))?;
    acquire_namespace_lock(&mut transaction, schema).await?;
    let status = manage_postgres_namespace_with_connection(
        destination,
        transaction.as_mut(),
        PostgresNamespaceAction::Validate,
    )
    .await?;
    anyhow::ensure!(
        status.version() == MAXIMUM_COMPATIBLE_SCHEMA_VERSION,
        "PostgreSQL Runtime State Namespace changed after migration preflight"
    );
    let progress = existing_progress(
        transaction.as_mut(),
        schema,
        source_identity,
        source_fingerprint,
    )
    .await?
    .context("Runtime State Migration must import threads before operational state")?;
    if progress.phase != RuntimeStateMigrationPhase::ThreadsImported {
        transaction
            .commit()
            .await
            .map_err(|error| map_sql_error(schema, "finish operational migration retry", error))?;
        return Ok(progress);
    }

    write_operational_state(transaction.as_mut(), schema, snapshot).await?;
    validate_operational_state(transaction.as_mut(), schema, snapshot).await?;
    let migration = qualified_table(schema, "runtime_state_migration");
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {migration} SET phase = 'operational_imported', ready = FALSE, \
         phase_evidence = '{{}}'::jsonb, fencing_token = 2, updated_at = CURRENT_TIMESTAMP \
         WHERE singleton"
    )))
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "record operational migration phase", error))?;
    let digest = namespace_digest(transaction.as_mut(), schema).await?;
    let evidence = phase_evidence(
        transaction.as_mut(),
        schema,
        RuntimeStateMigrationEvidence {
            source_identity,
            source_fingerprint,
            phase: RuntimeStateMigrationPhase::OperationalImported,
            ready: false,
            fencing_token: 2,
            namespace_digest: &digest,
        },
    )
    .await?;
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {migration} SET phase_evidence = $1 WHERE singleton"
    )))
    .bind(evidence)
    .execute(transaction.as_mut())
    .await
    .map_err(|error| map_sql_error(schema, "seal operational migration evidence", error))?;
    transaction
        .commit()
        .await
        .map_err(|error| map_sql_error(schema, "commit operational migration", error))?;
    Ok(RuntimeStateMigrationProgress {
        phase: RuntimeStateMigrationPhase::OperationalImported,
        fencing_token: 2,
    })
}

async fn validate_operational_state(
    connection: &mut sqlx::PgConnection,
    schema: &str,
    expected: &OperationalSnapshot,
) -> anyhow::Result<()> {
    let logs = qualified_table(schema, "logs");
    let goals = qualified_table(schema, "thread_goals");
    let deferrals = qualified_table(schema, "thread_goal_continuation_deferrals");
    let events = qualified_table(schema, "thread_goal_accounting_events");
    let enrollments = qualified_table(schema, "remote_control_enrollments");
    let imports = qualified_table(schema, "external_agent_config_imports");
    let actual = OperationalSnapshot {
        logs: sqlx::query_as(AssertSqlSafe(format!(
            "SELECT id, ts, ts_nanos, level, target, feedback_log_body, module_path, file, line, \
             thread_id, process_uuid, estimated_bytes FROM {logs} ORDER BY id"
        )))
        .fetch_all(&mut *connection)
        .await?,
        goals: sqlx::query_as(AssertSqlSafe(format!(
            "SELECT goals.thread_id, goals.goal_id, goals.objective, goals.status, \
             goals.token_budget, goals.tokens_used, goals.time_used_seconds, goals.created_at_ms, \
             goals.updated_at_ms, EXISTS(SELECT 1 FROM {deferrals} deferrals WHERE \
             deferrals.thread_id = goals.thread_id) AS continuation_deferred \
             FROM {goals} goals ORDER BY thread_id"
        )))
        .fetch_all(&mut *connection)
        .await?,
        accounting_events: sqlx::query_as(AssertSqlSafe(format!(
            "SELECT thread_id, event_id, goal_id, time_delta_seconds, token_delta, mode \
             FROM {events} ORDER BY thread_id, event_id"
        )))
        .fetch_all(&mut *connection)
        .await?,
        enrollments: sqlx::query_as(AssertSqlSafe(format!(
            "SELECT websocket_url, account_id, app_server_client_name, server_id, environment_id, \
             server_name, remote_control_enabled, updated_at FROM {enrollments} \
             ORDER BY websocket_url, account_id, app_server_client_name"
        )))
        .fetch_all(&mut *connection)
        .await?,
        imports: sqlx::query_as(AssertSqlSafe(format!(
            "SELECT import_id, provider_id, completed_at_ms, successes, failures \
             FROM {imports} ORDER BY import_id"
        )))
        .fetch_all(&mut *connection)
        .await?,
    };
    anyhow::ensure!(
        actual == *expected,
        "imported operational Runtime State does not exactly match its SQLite source snapshot"
    );
    Ok(())
}

async fn write_operational_state(
    connection: &mut sqlx::PgConnection,
    schema: &str,
    snapshot: &OperationalSnapshot,
) -> anyhow::Result<()> {
    let logs = qualified_table(schema, "logs");
    for row in &snapshot.logs {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {logs} (id, ts, ts_nanos, level, target, feedback_log_body, module_path, \
             file, line, thread_id, process_uuid, estimated_bytes) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"
        )))
        .bind(row.id)
        .bind(row.ts)
        .bind(row.ts_nanos)
        .bind(&row.level)
        .bind(&row.target)
        .bind(&row.feedback_log_body)
        .bind(&row.module_path)
        .bind(&row.file)
        .bind(row.line)
        .bind(&row.thread_id)
        .bind(&row.process_uuid)
        .bind(row.estimated_bytes)
        .execute(&mut *connection)
        .await
        .map_err(|error| map_sql_error(schema, "import operational logs", error))?;
    }
    sqlx::query(AssertSqlSafe(format!(
        "SELECT setval(pg_get_serial_sequence($1, 'id'), COALESCE(MAX(id), 1), COUNT(*) > 0) \
         FROM {logs}"
    )))
    .bind(format!("{}.logs", quote_identifier(schema)))
    .execute(&mut *connection)
    .await
    .map_err(|error| map_sql_error(schema, "advance imported log identifiers", error))?;

    let goals = qualified_table(schema, "thread_goals");
    let deferrals = qualified_table(schema, "thread_goal_continuation_deferrals");
    for row in &snapshot.goals {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {goals} (thread_id, goal_id, objective, status, token_budget, \
             tokens_used, time_used_seconds, created_at_ms, updated_at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"
        )))
        .bind(&row.thread_id)
        .bind(&row.goal_id)
        .bind(&row.objective)
        .bind(&row.status)
        .bind(row.token_budget)
        .bind(row.tokens_used)
        .bind(row.time_used_seconds)
        .bind(row.created_at_ms)
        .bind(row.updated_at_ms)
        .execute(&mut *connection)
        .await
        .map_err(|error| map_sql_error(schema, "import operational goals", error))?;
        if row.continuation_deferred {
            sqlx::query(AssertSqlSafe(format!(
                "INSERT INTO {deferrals} (thread_id) VALUES ($1)"
            )))
            .bind(&row.thread_id)
            .execute(&mut *connection)
            .await
            .map_err(|error| map_sql_error(schema, "import goal deferrals", error))?;
        }
    }
    let events = qualified_table(schema, "thread_goal_accounting_events");
    for row in &snapshot.accounting_events {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {events} (thread_id, event_id, goal_id, time_delta_seconds, token_delta, mode) \
             VALUES ($1, $2, $3, $4, $5, $6)"
        )))
        .bind(&row.thread_id)
        .bind(&row.event_id)
        .bind(&row.goal_id)
        .bind(row.time_delta_seconds)
        .bind(row.token_delta)
        .bind(&row.mode)
        .execute(&mut *connection)
        .await
        .map_err(|error| map_sql_error(schema, "import goal accounting events", error))?;
    }

    let enrollments = qualified_table(schema, "remote_control_enrollments");
    for row in &snapshot.enrollments {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {enrollments} (websocket_url, account_id, app_server_client_name, \
             server_id, environment_id, server_name, remote_control_enabled, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"
        )))
        .bind(&row.websocket_url)
        .bind(&row.account_id)
        .bind(&row.app_server_client_name)
        .bind(&row.server_id)
        .bind(&row.environment_id)
        .bind(&row.server_name)
        .bind(row.remote_control_enabled)
        .bind(row.updated_at)
        .execute(&mut *connection)
        .await
        .map_err(|error| map_sql_error(schema, "import remote-control enrollments", error))?;
    }

    let imports = qualified_table(schema, "external_agent_config_imports");
    for row in &snapshot.imports {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {imports} (import_id, provider_id, completed_at_ms, successes, failures) \
             VALUES ($1, $2, $3, $4, $5)"
        )))
        .bind(&row.import_id)
        .bind(&row.provider_id)
        .bind(row.completed_at_ms)
        .bind(&row.successes)
        .bind(&row.failures)
        .execute(&mut *connection)
        .await
        .map_err(|error| map_sql_error(schema, "import external-agent outcomes", error))?;
    }
    Ok(())
}
