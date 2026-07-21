use clap::Args;
use clap::Parser;
use codex_state::PostgresNamespaceAction;
use codex_state::PostgresNamespaceConfig;
use codex_state::PostgresNamespaceStatus;
use codex_state::PostgresPoolConfig;
use codex_state::manage_postgres_namespace;
use std::num::NonZeroU32;
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

pub async fn run(command: StateCommand) -> anyhow::Result<()> {
    let (action, args) = match command.subcommand {
        StateSubcommand::Schema(command) => match command.action {
            PostgresSchemaAction::Migrate(args) => (PostgresNamespaceAction::Migrate, args),
            PostgresSchemaAction::Validate(args) => (PostgresNamespaceAction::Validate, args),
        },
    };
    let pool = PostgresPoolConfig::new(
        args.max_connections,
        Duration::from_millis(args.acquire_timeout_ms),
        Duration::from_millis(args.statement_timeout_ms),
    )?;
    let config = PostgresNamespaceConfig::new(args.url_env, args.schema, pool)?;
    let status = manage_postgres_namespace(config, action).await?;
    print_status(action, status);
    Ok(())
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
