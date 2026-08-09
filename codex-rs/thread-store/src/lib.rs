//! Storage-neutral thread persistence interfaces.
//!
//! Application code should treat [`codex_protocol::ThreadId`] as the only durable thread handle.
//! Implementations are responsible for resolving that id to local rollout files, RPC requests, or
//! any other backing store.

mod error;
mod in_memory;
#[cfg(test)]
#[path = "lifecycle_contract_tests.rs"]
mod lifecycle_contract_tests;
#[cfg(test)]
#[path = "list_threads_contract_tests.rs"]
mod list_threads_contract_tests;
mod live_thread;
mod local;
#[cfg(test)]
#[path = "metadata_contract_tests.rs"]
mod metadata_contract_tests;
#[cfg(test)]
#[path = "model_context_contract_tests.rs"]
mod model_context_contract_tests;
mod occurrence_search;
mod postgres;
#[cfg(test)]
#[path = "postgres_contract_tests.rs"]
mod postgres_contract_tests;
#[cfg(test)]
#[path = "postgres_item_projection_contract_tests.rs"]
mod postgres_item_projection_contract_tests;
#[cfg(test)]
#[path = "postgres_lease_recovery_contract_tests.rs"]
mod postgres_lease_recovery_contract_tests;
#[cfg(test)]
#[path = "postgres_turn_projection_contract_tests.rs"]
mod postgres_turn_projection_contract_tests;
mod queue_store;
#[cfg(test)]
#[path = "runtime_state_migration_contract_tests.rs"]
mod runtime_state_migration_contract_tests;
#[cfg(test)]
#[path = "runtime_state_migration_rejection_contract_tests.rs"]
mod runtime_state_migration_rejection_contract_tests;
#[cfg(test)]
#[path = "search_thread_occurrences_contract_tests.rs"]
mod search_thread_occurrences_contract_tests;
#[cfg(test)]
#[path = "search_threads_contract_tests.rs"]
mod search_threads_contract_tests;
mod store;
mod thread_metadata_sync;
mod thread_sections;
mod types;

pub use codex_state::MAX_QUEUE_ITEMS;
pub use codex_state::QueuedUserSubmissionRecord;
pub use error::ThreadStoreError;
pub use error::ThreadStoreResult;
pub use in_memory::InMemoryThreadStore;
pub use in_memory::InMemoryThreadStoreCalls;
pub use live_thread::LiveThread;
pub use live_thread::LiveThreadInitGuard;
pub use local::LocalThreadStore;
pub use local::LocalThreadStoreConfig;
pub use local::RolloutMigrationMode;
pub use local::RolloutMigrationOptions;
pub use local::RolloutMigrationOutcome;
pub use local::RolloutMigrationProgress;
pub use local::RolloutMigrationReport;
pub use local::RolloutMigrationStatus;
pub use postgres::PostgresThreadProjectionMaterializer;
pub use postgres::PostgresThreadStore;
pub use queue_store::LocalQueueStore;
pub use queue_store::QueueStore;
pub use store::ChildRegistrationGuard;
pub use store::ThreadStore;
pub use store::ThreadStoreFuture;
pub use thread_sections::CreateThreadSectionParams;
pub use thread_sections::DeleteThreadSectionParams;
pub use thread_sections::ListThreadSectionsParams;
pub use thread_sections::RenameThreadSectionParams;
pub use thread_sections::StoredThreadSection;
pub use thread_sections::StoredThreadSectionsPage;
pub use types::AppendBatchCommit;
pub use types::AppendBatchId;
pub use types::AppendThreadItemsBatch;
pub use types::AppendThreadItemsParams;
pub use types::ArchiveThreadParams;
pub use types::ArchiveThreadsParams;
pub use types::ClearableField;
pub use types::CreateThreadParams;
pub use types::DeleteThreadParams;
pub use types::DeleteThreadsParams;
pub use types::ExtraConfig;
pub use types::ForkBoundary;
pub use types::GitInfoPatch;
pub use types::ItemPage;
pub use types::ItemSortKey;
pub use types::ListItemsParams;
pub use types::ListThreadsParams;
pub use types::ListTurnsParams;
pub use types::LoadThreadHistoryParams;
pub use types::MoveThreadToSectionParams;
pub use types::PrepareForkParams;
pub use types::PreparedFork;
pub use types::ReadThreadByRolloutPathParams;
pub use types::ReadThreadParams;
pub use types::ResumeThreadParams;
pub use types::SearchTextRange;
pub use types::SearchThreadOccurrencesParams;
pub use types::SearchThreadsParams;
pub use types::SortDirection;
pub use types::StoredModelContext;
pub use types::StoredThread;
pub use types::StoredThreadHistory;
pub use types::StoredThreadItem;
pub use types::StoredThreadOccurrence;
pub use types::StoredThreadSearchResult;
pub use types::StoredTurn;
pub use types::StoredTurnError;
pub use types::StoredTurnItemsView;
pub use types::StoredTurnStatus;
pub use types::ThreadLocationFilter;
pub use types::ThreadMetadataPatch;
pub use types::ThreadOccurrenceSearchPage;
pub use types::ThreadPage;
pub use types::ThreadPersistenceMetadata;
pub use types::ThreadRelationFilter;
pub use types::ThreadSearchPage;
pub use types::ThreadSortKey;
pub use types::TurnPage;
pub use types::UpdateThreadMetadataParams;
