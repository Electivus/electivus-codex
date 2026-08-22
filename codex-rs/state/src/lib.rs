//! Durable runtime state backed by SQLite or PostgreSQL.
//!
//! The selected backend owns thread metadata, logs, goals, memories, remote-control
//! enrollment, and related runtime state. SQLite remains the local default; a ready
//! PostgreSQL namespace can be selected as the integral shared backend.

const _: () = assert!(
    libsqlite3_sys::SQLITE_VERSION_NUMBER >= 3_051_003,
    "bundled SQLite must include the WAL-reset corruption fix",
);

mod audit;
mod extract;
pub mod log_db;
mod migration;
mod migrations;
mod model;
mod paths;
mod postgres;
mod runtime;
mod sqlite;
mod telemetry;

pub use migration::BackfillCoordinationSnapshot;
pub use migration::CanonicalThreadHistoryReader;
pub use migration::CanonicalThreadHistorySnapshot;
pub use migration::RuntimeStateMigrationInventory;
pub use migration::RuntimeStateMigrationPhase;
pub use migration::RuntimeStateMigrationProgress;
pub use migration::RuntimeStateMigrationReport;
pub use migration::RuntimeStateThreadProjectionMaterializer;
pub use migration::RuntimeStateThreadSnapshot;
pub use migration::SourceFileInventory;
pub use migration::SqliteDatabaseInventory;
pub use migration::SqliteTableInventory;
pub use migration::ThreadHistoryProjectionStateSnapshot;
pub use migration::ThreadItemProjectionSnapshot;
pub use migration::ThreadMigrationSnapshot;
pub use migration::ThreadSpawnEdgeSnapshot;
pub use migration::ThreadTurnProjectionSnapshot;
pub use migration::import_runtime_state_memory;
pub use migration::import_runtime_state_operational;
pub use migration::import_runtime_state_threads;
pub use migration::migrate_runtime_state;
pub use migration::preflight_runtime_state_migration;
pub use migration::snapshot_runtime_state_migration_threads;
pub use model::CreatedProject;
pub use model::LogEntry;
pub use model::LogQuery;
pub use model::LogRow;
pub use model::Phase2JobClaimOutcome;
pub use model::Project;
pub use model::ProjectRoot;
pub use model::ProjectsPage;
pub use model::QueuedUserSubmissionRecord;
pub use model::RolloutMigrationCursor;
pub use model::RolloutMigrationSkippedRollout;
pub use model::RolloutMigrationState;
pub use postgres::PostgresNamespaceAction;
pub use postgres::PostgresNamespaceConfig;
pub use postgres::PostgresNamespaceStatus;
pub use postgres::PostgresPoolConfig;
pub use postgres::PostgresRuntimeStatePool;
pub use postgres::RuntimeStateInitializationReport;
pub use postgres::initialize_postgres_runtime_state;
pub use postgres::manage_postgres_namespace;
pub use runtime::BackfillClaimOutcome;
pub use runtime::BackfillCoordinator;
pub use runtime::BackfillLease;
pub use runtime::BackfillLeaseUpdate;
pub use runtime::MemoryArtifact;
pub use runtime::MemoryArtifactSet;
pub use runtime::MemoryGeneration;
pub use runtime::MemoryWorkspaceMaterialization;
pub use runtime::RuntimeStateBackendConfig;
/// Preferred entrypoint: owns configuration and metrics.
pub use runtime::StateRuntime;
pub use sqlite::SqliteConfig;

pub use audit::ThreadStateAuditRow;
pub use audit::read_thread_state_audit_rows;
/// Low-level storage engine: useful for focused tests.
///
/// Most consumers should prefer [`StateRuntime`].
pub use extract::apply_rollout_item;
pub use extract::rollout_item_affects_thread_metadata;
pub use model::Anchor;
pub use model::BackfillState;
pub use model::BackfillStats;
pub use model::BackfillStatus;
pub use model::DirectionalThreadSpawnEdgeStatus;
pub use model::ExtractionOutcome;
pub use model::SortDirection;
pub use model::SortKey;
pub use model::Stage1JobClaim;
pub use model::Stage1JobClaimOutcome;
pub use model::Stage1Output;
pub use model::Stage1StartupClaimParams;
pub use model::ThreadGoal;
pub use model::ThreadGoalStatus;
pub use model::ThreadMetadata;
pub use model::ThreadMetadataBuilder;
pub use model::ThreadRelationFilter;
pub use model::ThreadSection;
pub use model::ThreadSectionAppearance;
pub use model::ThreadSectionsPage;
pub use model::ThreadsPage;
pub use runtime::ExternalAgentConfigImportDetailsRecord;
pub use runtime::ExternalAgentConfigImportFailureRecord;
pub use runtime::ExternalAgentConfigImportHistoryRecord;
pub use runtime::ExternalAgentConfigImportStore;
pub use runtime::ExternalAgentConfigImportSuccessRecord;
pub use runtime::ExternalAgentMemoryImport;
pub use runtime::GoalAccountingMode;
pub use runtime::GoalAccountingOutcome;
pub use runtime::GoalAccountingRequest;
pub use runtime::GoalAccountingTarget;
pub use runtime::GoalStore;
pub use runtime::GoalStoreError;
pub use runtime::GoalStoreErrorKind;
pub use runtime::GoalStoreOperation;
pub use runtime::GoalStoreResult;
pub use runtime::GoalUpdate;
pub use runtime::MemoryStore;
pub use runtime::RemoteControlEnrollmentRecord;
pub use runtime::RemoteControlEnrollmentStore;
pub use runtime::RuntimeDbBackup;
pub use runtime::SqliteQueueStore;
pub use runtime::ThreadFilterOptions;
pub use runtime::ThreadResumeMetadata;
pub use runtime::backup_runtime_db_for_fresh_start;
pub use runtime::is_sqlite_corruption_error;
pub use runtime::open_thread_history_db;
pub use runtime::runtime_db_path_for_corruption_error;
pub use runtime::sqlite_error_detail_is_corruption;
pub use runtime::sqlite_error_detail_is_lock;
pub use runtime::sqlite_integrity_check;
pub use sqlite::RuntimeDbPath;
pub use telemetry::DbTelemetry;
pub use telemetry::DbTelemetryHandle;
pub use telemetry::install_process_db_telemetry;
pub use telemetry::record_backfill_gate;
pub use telemetry::record_fallback;

/// Maximum number of pending user submissions permitted for one thread.
pub const MAX_QUEUE_ITEMS: usize = 100;

/// Stable UUIDv7 identifying the built-in pinned thread section.
pub const PINNED_THREAD_SECTION_ID: &str = "01984de2-8f74-7c91-a3b2-5c5e937cf318";

/// User-facing name of the built-in pinned thread section.
pub const PINNED_THREAD_SECTION_NAME: &str = "Pinned";

/// Environment variable for overriding the SQLite state database home directory.
pub const SQLITE_HOME_ENV: &str = "CODEX_SQLITE_HOME";

/// Errors encountered during DB operations. Tags: [stage]
pub const DB_ERROR_METRIC: &str = "codex.db.error";
/// Metrics on backfill process. Tags: [status]
pub const DB_METRIC_BACKFILL: &str = "codex.db.backfill";
/// Metrics on backfill duration. Tags: [status]
pub const DB_METRIC_BACKFILL_DURATION_MS: &str = "codex.db.backfill.duration_ms";
/// SQLite initialization attempts. Tags: [status, phase, db, error]
pub const DB_INIT_METRIC: &str = "codex.sqlite.init.count";
/// SQLite initialization latency. Tags: [status, phase, db, error]
pub const DB_INIT_DURATION_METRIC: &str = "codex.sqlite.init.duration_ms";
/// Rollout fallback attempts. Tags: [caller, reason]
pub const DB_FALLBACK_METRIC: &str = "codex.sqlite.fallback.count";
