use clap::ArgGroup;
use clap::Args;
use clap::Parser;
use codex_core::config::ConfigBuilder;
use codex_state::PostgresNamespaceAction;
use codex_state::PostgresNamespaceConfig;
use codex_state::PostgresNamespaceStatus;
use codex_state::PostgresPoolConfig;
use codex_state::RuntimeStateBackendConfig;
use codex_state::manage_postgres_namespace;
use codex_thread_store::PostgresThreadProjectionMaterializer;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cli::CliConfigOverrides;
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
#[command(group(
    ArgGroup::new("connection_source")
        .args(["url", "url_env"])
        .multiple(false)
))]
struct PostgresNamespaceArgs {
    /// Direct passwordless PostgreSQL URL with `sslmode=verify-full` and absolute `sslrootcert`, `sslcert`, and `sslkey` paths.
    #[arg(long, value_name = "URL", conflicts_with = "url_env")]
    url: Option<String>,

    /// Environment variable containing a passwordless PostgreSQL URL with `sslmode=verify-full` and absolute `sslrootcert`, `sslcert`, and `sslkey` paths.
    #[arg(long, value_name = "ENV_VAR", conflicts_with = "url")]
    url_env: Option<String>,

    /// PostgreSQL schema containing the Runtime State Namespace (defaults to `codex` with an explicit source).
    #[arg(long, requires = "connection_source")]
    schema: Option<String>,

    /// Maximum number of connections in this namespace pool (defaults to 10 with an explicit source).
    #[arg(long, requires = "connection_source")]
    max_connections: Option<NonZeroU32>,

    /// Maximum time to wait for a pooled connection, in milliseconds (defaults to 10000 with an explicit source).
    #[arg(long, requires = "connection_source")]
    acquire_timeout_ms: Option<u64>,

    /// PostgreSQL statement timeout, in milliseconds (defaults to 30000 with an explicit source).
    #[arg(long, requires = "connection_source")]
    statement_timeout_ms: Option<u64>,
}

#[derive(Debug, Args)]
struct RuntimeStateMigrationArgs {
    /// Complete, offline SQLite home to preserve and migrate.
    #[arg(long, value_name = "PATH")]
    sqlite_home: PathBuf,

    #[command(flatten)]
    destination: PostgresNamespaceArgs,
}

pub async fn run(
    command: StateCommand,
    config_overrides: CliConfigOverrides,
) -> anyhow::Result<()> {
    match command.subcommand {
        StateSubcommand::Schema(command) => run_schema(command, &config_overrides).await,
        StateSubcommand::Initialize(args) => run_initialize(args, &config_overrides).await,
        StateSubcommand::Migrate(args) => run_migration(args, &config_overrides).await,
    }
}

async fn run_schema(
    command: PostgresSchemaCommand,
    config_overrides: &CliConfigOverrides,
) -> anyhow::Result<()> {
    let (action, args) = match command.action {
        PostgresSchemaAction::Migrate(args) => (PostgresNamespaceAction::Migrate, args),
        PostgresSchemaAction::Validate(args) => (PostgresNamespaceAction::Validate, args),
    };
    let config = postgres_config(args, config_overrides).await?;
    let status = manage_postgres_namespace(config, action).await?;
    print_status(action, status);
    Ok(())
}

async fn run_initialize(
    args: PostgresNamespaceArgs,
    config_overrides: &CliConfigOverrides,
) -> anyhow::Result<()> {
    let report = codex_state::initialize_postgres_runtime_state(
        postgres_config(args, config_overrides).await?,
    )
    .await?;
    let output =
        format_initialization_success(report.schema(), report.fencing_token(), report.evidence())?;
    print!("{output}");
    Ok(())
}

async fn run_migration(
    args: RuntimeStateMigrationArgs,
    config_overrides: &CliConfigOverrides,
) -> anyhow::Result<()> {
    let source =
        codex_state::SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(args.sqlite_home)?);
    let destination = postgres_config(args.destination, config_overrides).await?;
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

async fn postgres_config(
    args: PostgresNamespaceArgs,
    config_overrides: &CliConfigOverrides,
) -> anyhow::Result<PostgresNamespaceConfig> {
    let source = match (args.url, args.url_env) {
        (Some(url), None) => Some(PostgresCliConnectionSource::Direct(url)),
        (None, Some(url_env)) => Some(PostgresCliConnectionSource::Environment(url_env)),
        (None, None) => None,
        (Some(_), Some(_)) => anyhow::bail!("`--url` and `--url-env` are mutually exclusive"),
    };
    let Some(source) = source else {
        let cli_overrides = config_overrides
            .parse_overrides()
            .map_err(anyhow::Error::msg)?;
        let config = ConfigBuilder::default()
            .cli_overrides(cli_overrides)
            .build()
            .await?;
        return match config.runtime_state_backend {
            RuntimeStateBackendConfig::Postgresql { namespace, .. } => Ok(namespace),
            RuntimeStateBackendConfig::Sqlite(_) => anyhow::bail!(
                "PostgreSQL Runtime State is not configured; select `state.backend = \"postgresql\"` in config.toml or provide `--url <URL>` or `--url-env <ENV_VAR>`"
            ),
            _ => anyhow::bail!("the configured Runtime State Backend is not PostgreSQL"),
        };
    };
    let pool = PostgresPoolConfig::new(
        args.max_connections
            .unwrap_or_else(|| NonZeroU32::new(10).unwrap_or(NonZeroU32::MIN)),
        Duration::from_millis(args.acquire_timeout_ms.unwrap_or(10_000)),
        Duration::from_millis(args.statement_timeout_ms.unwrap_or(30_000)),
    )?;
    let schema = args.schema.unwrap_or_else(|| "codex".to_string());
    match source {
        PostgresCliConnectionSource::Direct(url) => {
            PostgresNamespaceConfig::new_with_cli_url(url, schema, pool)
        }
        PostgresCliConnectionSource::Environment(url_env) => {
            PostgresNamespaceConfig::new(url_env, schema, pool)
        }
    }
}

enum PostgresCliConnectionSource {
    Direct(String),
    Environment(String),
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
