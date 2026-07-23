use clap::Args;
use clap::Parser;
use codex_state::PostgresNamespaceAction;
use codex_state::PostgresNamespaceConfig;
use codex_state::PostgresNamespaceStatus;
use codex_state::PostgresPoolConfig;
use codex_state::manage_postgres_namespace;
use codex_thread_store::PostgresThreadProjectionMaterializer;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Duration;

/// Manage the Runtime State Store.
#[derive(Debug, Parser)]
pub struct StateCommand {
    #[command(subcommand)]
    subcommand: StateSubcommand,
}

#[derive(Debug, clap::Subcommand)]
enum StateSubcommand {
    /// Explicitly manage a PostgreSQL Runtime State Namespace schema.
    Schema(PostgresSchemaCommand),
    /// Initialize a new, empty PostgreSQL Runtime State Namespace.
    Initialize(PostgresNamespaceArgs),
    /// Migrate one offline SQLite Runtime State Namespace into empty PostgreSQL.
    Migrate(RuntimeStateMigrationArgs),
}

#[derive(Debug, Parser)]
struct PostgresSchemaCommand {
    #[command(subcommand)]
    action: PostgresSchemaAction,
}

#[derive(Debug, clap::Subcommand)]
enum PostgresSchemaAction {
    /// Create the configured schema when absent and apply migrations.
    Migrate(PostgresNamespaceArgs),
    /// Validate server and schema compatibility without changing the schema.
    Validate(PostgresNamespaceArgs),
}

#[derive(Debug, Args)]
struct PostgresNamespaceArgs {
    /// Environment variable containing the PostgreSQL connection URL.
    #[arg(long, value_name = "ENV_VAR")]
    url_env: String,

    /// PostgreSQL schema containing the Runtime State Namespace.
    #[arg(long, default_value = "codex")]
    schema: String,

    /// Maximum number of connections in this namespace pool.
    #[arg(long, default_value = "10")]
    max_connections: NonZeroU32,

    /// Maximum time to wait for a pooled connection, in milliseconds.
    #[arg(long, default_value_t = 10_000)]
    acquire_timeout_ms: u64,

    /// PostgreSQL statement timeout, in milliseconds.
    #[arg(long, default_value_t = 30_000)]
    statement_timeout_ms: u64,
}

#[derive(Debug, Args)]
struct RuntimeStateMigrationArgs {
    /// Complete, offline SQLite home to preserve and migrate.
    #[arg(long, value_name = "PATH")]
    sqlite_home: PathBuf,

    #[command(flatten)]
    destination: PostgresNamespaceArgs,
}

pub async fn run(command: StateCommand) -> anyhow::Result<()> {
    match command.subcommand {
        StateSubcommand::Schema(command) => run_schema(command).await,
        StateSubcommand::Initialize(args) => run_initialize(args).await,
        StateSubcommand::Migrate(args) => run_migration(args).await,
    }
}

async fn run_schema(command: PostgresSchemaCommand) -> anyhow::Result<()> {
    let (action, args) = match command.action {
        PostgresSchemaAction::Migrate(args) => (PostgresNamespaceAction::Migrate, args),
        PostgresSchemaAction::Validate(args) => (PostgresNamespaceAction::Validate, args),
    };
    let config = postgres_config(args)?;
    let status = manage_postgres_namespace(config, action).await?;
    print_status(action, status);
    Ok(())
}

async fn run_initialize(args: PostgresNamespaceArgs) -> anyhow::Result<()> {
    let report = codex_state::initialize_postgres_runtime_state(postgres_config(args)?).await?;
    let output =
        format_initialization_success(report.schema(), report.fencing_token(), report.evidence())?;
    print!("{output}");
    Ok(())
}

async fn run_migration(args: RuntimeStateMigrationArgs) -> anyhow::Result<()> {
    let source =
        codex_state::SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(args.sqlite_home)?);
    let destination = postgres_config(args.destination)?;
    let projection_materializer = PostgresThreadProjectionMaterializer::new(&destination);
    let report = codex_state::migrate_runtime_state(
        source,
        destination,
        &codex_rollout::CanonicalRolloutHistoryReader,
        &projection_materializer,
    )
    .await?;
    let output = format_migration_success(
        report.destination_schema(),
        report.fencing_token(),
        report.evidence(),
    )?;
    print!("{output}");
    Ok(())
}

pub(super) fn format_initialization_success(
    destination_schema: &str,
    fencing_token: i64,
    evidence: &serde_json::Value,
) -> anyhow::Result<String> {
    let evidence = serde_json::to_string(evidence)?;
    Ok(format!(
        "PostgreSQL Runtime State Namespace `{destination_schema}` was initialized empty and is READY at readiness fence {fencing_token}.\n\
         Validated the current schema layout, empty authoritative stores, referential integrity, and an active empty Memory Generation.\n\
         Readiness evidence: {evidence}\n\
         No SQLite Runtime State Namespace was read or migrated.\n\
         config.toml was not changed; select the PostgreSQL backend separately after review.\n"
    ))
}

pub(super) fn format_migration_success(
    destination_schema: &str,
    fencing_token: i64,
    evidence: &serde_json::Value,
) -> anyhow::Result<String> {
    let evidence = serde_json::to_string(evidence)?;
    Ok(format!(
        "PostgreSQL Runtime State Namespace `{destination_schema}` is READY at migration fence {fencing_token}.\n\
         Validated counts, identifiers, ordering, content hashes, referential integrity, and every Runtime State Store responsibility.\n\
         Integrity evidence: {evidence}\n\
         The SQLite source, rollouts, Memory Artifacts, and config.toml were preserved.\n\
         config.toml was not changed; select the PostgreSQL backend separately after review.\n\
         WARNING: This migration is forward-only. After PostgreSQL accepts new writes, the preserved SQLite source becomes stale and cannot provide a lossless rollback.\n"
    ))
}

fn postgres_config(args: PostgresNamespaceArgs) -> anyhow::Result<PostgresNamespaceConfig> {
    let pool = PostgresPoolConfig::new(
        args.max_connections,
        Duration::from_millis(args.acquire_timeout_ms),
        Duration::from_millis(args.statement_timeout_ms),
    )?;
    PostgresNamespaceConfig::new(args.url_env, args.schema, pool)
}

fn print_status(action: PostgresNamespaceAction, status: PostgresNamespaceStatus) {
    let operation = match action {
        PostgresNamespaceAction::Migrate => "migrated",
        PostgresNamespaceAction::Validate => "validated",
    };
    let compatible_versions = status.compatible_versions();
    println!(
        "PostgreSQL Runtime State Namespace `{}` {operation} at schema version {} (supported {}..={}).",
        status.schema(),
        status.version(),
        compatible_versions.start(),
        compatible_versions.end(),
    );
}
