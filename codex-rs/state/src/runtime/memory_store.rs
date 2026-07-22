use super::memories::SqliteMemoryStore;
use crate::Phase2JobClaimOutcome;
use crate::Stage1JobClaim;
use crate::Stage1JobClaimOutcome;
use crate::Stage1Output;
use crate::Stage1StartupClaimParams;
use codex_protocol::ThreadId;
use sqlx::PgPool;
use sqlx::SqlitePool;
use std::sync::Arc;
#[path = "memory_store/postgres.rs"]
mod postgres;
use postgres::PostgresMemoryStore;

/// Storage-neutral facade for memory extraction and consolidation state.
#[derive(Clone)]
pub struct MemoryStore {
    backend: MemoryStoreBackend,
}

#[derive(Clone)]
enum MemoryStoreBackend {
    Postgres(PostgresMemoryStore),
    Sqlite(SqliteMemoryStore),
}

impl MemoryStore {
    pub(crate) fn new(pool: Arc<SqlitePool>, state_pool: Arc<SqlitePool>) -> Self {
        Self {
            backend: MemoryStoreBackend::Sqlite(SqliteMemoryStore::new(pool, state_pool)),
        }
    }
    pub(crate) fn from_postgres(pool: PgPool, schema: String) -> Self {
        Self {
            backend: MemoryStoreBackend::Postgres(PostgresMemoryStore::new(pool, schema)),
        }
    }
    pub(crate) async fn close(&self) {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => store.close().await,
            MemoryStoreBackend::Sqlite(store) => store.close().await,
        }
    }
    fn sqlite(&self, operation: &str) -> anyhow::Result<&SqliteMemoryStore> {
        match &self.backend {
            MemoryStoreBackend::Postgres(_) => anyhow::bail!(
                "PostgreSQL memory store does not yet implement `{operation}`; keep PostgreSQL Runtime State selection disabled"
            ),
            MemoryStoreBackend::Sqlite(store) => Ok(store),
        }
    }
    pub async fn clear_memory_data(&self) -> anyhow::Result<()> {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => store.clear_memory_data().await,
            MemoryStoreBackend::Sqlite(store) => store.clear_memory_data().await,
        }
    }
    pub async fn record_stage1_output_usage(
        &self,
        thread_ids: &[ThreadId],
    ) -> anyhow::Result<usize> {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => {
                store.record_stage1_output_usage(thread_ids).await
            }
            MemoryStoreBackend::Sqlite(store) => store.record_stage1_output_usage(thread_ids).await,
        }
    }
    pub async fn claim_stage1_jobs_for_startup(
        &self,
        current_thread_id: ThreadId,
        params: Stage1StartupClaimParams<'_>,
    ) -> anyhow::Result<Vec<Stage1JobClaim>> {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => {
                store
                    .claim_stage1_jobs_for_startup(current_thread_id, params)
                    .await
            }
            MemoryStoreBackend::Sqlite(store) => {
                store
                    .claim_stage1_jobs_for_startup(current_thread_id, params)
                    .await
            }
        }
    }
    pub(super) async fn delete_thread_memory(&self, thread_id: ThreadId) -> anyhow::Result<()> {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => store.delete_thread_memory(thread_id).await,
            MemoryStoreBackend::Sqlite(store) => store.delete_thread_memory(thread_id).await,
        }
    }
    pub async fn list_stage1_outputs_for_global(
        &self,
        n: usize,
    ) -> anyhow::Result<Vec<Stage1Output>> {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => store.list_stage1_outputs_for_global(n).await,
            MemoryStoreBackend::Sqlite(store) => store.list_stage1_outputs_for_global(n).await,
        }
    }

    pub async fn prune_stage1_outputs_for_retention(
        &self,
        max_unused_days: i64,
        limit: usize,
    ) -> anyhow::Result<usize> {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => {
                store
                    .prune_stage1_outputs_for_retention(max_unused_days, limit)
                    .await
            }
            MemoryStoreBackend::Sqlite(store) => {
                store
                    .prune_stage1_outputs_for_retention(max_unused_days, limit)
                    .await
            }
        }
    }

    pub async fn get_phase2_input_selection(
        &self,
        n: usize,
        max_unused_days: i64,
    ) -> anyhow::Result<Vec<Stage1Output>> {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => {
                store.get_phase2_input_selection(n, max_unused_days).await
            }
            MemoryStoreBackend::Sqlite(store) => {
                store.get_phase2_input_selection(n, max_unused_days).await
            }
        }
    }

    pub async fn mark_thread_memory_mode_polluted(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<bool> {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => {
                store.mark_thread_memory_mode_polluted(thread_id).await
            }
            MemoryStoreBackend::Sqlite(store) => {
                store.mark_thread_memory_mode_polluted(thread_id).await
            }
        }
    }

    pub async fn try_claim_stage1_job(
        &self,
        thread_id: ThreadId,
        worker_id: ThreadId,
        source_updated_at: i64,
        lease_seconds: i64,
        max_running_jobs: usize,
    ) -> anyhow::Result<Stage1JobClaimOutcome> {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => {
                store
                    .try_claim_stage1_job(
                        thread_id,
                        worker_id,
                        source_updated_at,
                        lease_seconds,
                        max_running_jobs,
                    )
                    .await
            }
            MemoryStoreBackend::Sqlite(store) => {
                store
                    .try_claim_stage1_job(
                        thread_id,
                        worker_id,
                        source_updated_at,
                        lease_seconds,
                        max_running_jobs,
                    )
                    .await
            }
        }
    }

    pub async fn mark_stage1_job_succeeded(
        &self,
        thread_id: ThreadId,
        ownership_token: &str,
        source_updated_at: i64,
        raw_memory: &str,
        rollout_summary: &str,
        rollout_slug: Option<&str>,
    ) -> anyhow::Result<bool> {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => {
                store
                    .mark_stage1_job_succeeded(
                        thread_id,
                        ownership_token,
                        source_updated_at,
                        raw_memory,
                        rollout_summary,
                        rollout_slug,
                    )
                    .await
            }
            MemoryStoreBackend::Sqlite(store) => {
                store
                    .mark_stage1_job_succeeded(
                        thread_id,
                        ownership_token,
                        source_updated_at,
                        raw_memory,
                        rollout_summary,
                        rollout_slug,
                    )
                    .await
            }
        }
    }

    pub async fn mark_stage1_job_succeeded_no_output(
        &self,
        thread_id: ThreadId,
        ownership_token: &str,
    ) -> anyhow::Result<bool> {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => {
                store
                    .mark_stage1_job_succeeded_no_output(thread_id, ownership_token)
                    .await
            }
            MemoryStoreBackend::Sqlite(store) => {
                store
                    .mark_stage1_job_succeeded_no_output(thread_id, ownership_token)
                    .await
            }
        }
    }

    pub async fn heartbeat_stage1_job(
        &self,
        thread_id: ThreadId,
        ownership_token: &str,
        lease_seconds: i64,
    ) -> anyhow::Result<bool> {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => {
                store
                    .heartbeat_stage1_job(thread_id, ownership_token, lease_seconds)
                    .await
            }
            MemoryStoreBackend::Sqlite(store) => {
                store
                    .heartbeat_stage1_job(thread_id, ownership_token, lease_seconds)
                    .await
            }
        }
    }

    pub async fn mark_stage1_job_failed(
        &self,
        thread_id: ThreadId,
        ownership_token: &str,
        failure_reason: &str,
        retry_delay_seconds: i64,
    ) -> anyhow::Result<bool> {
        match &self.backend {
            MemoryStoreBackend::Postgres(store) => {
                store
                    .mark_stage1_job_failed(
                        thread_id,
                        ownership_token,
                        failure_reason,
                        retry_delay_seconds,
                    )
                    .await
            }
            MemoryStoreBackend::Sqlite(store) => {
                store
                    .mark_stage1_job_failed(
                        thread_id,
                        ownership_token,
                        failure_reason,
                        retry_delay_seconds,
                    )
                    .await
            }
        }
    }

    pub async fn enqueue_global_consolidation(&self, input_watermark: i64) -> anyhow::Result<()> {
        self.sqlite("enqueue global memory consolidation")?
            .enqueue_global_consolidation(input_watermark)
            .await
    }

    pub async fn try_claim_global_phase2_job(
        &self,
        worker_id: ThreadId,
        lease_seconds: i64,
    ) -> anyhow::Result<Phase2JobClaimOutcome> {
        self.sqlite("claim global memory consolidation")?
            .try_claim_global_phase2_job(worker_id, lease_seconds)
            .await
    }

    pub async fn heartbeat_global_phase2_job(
        &self,
        ownership_token: &str,
        lease_seconds: i64,
    ) -> anyhow::Result<bool> {
        self.sqlite("heartbeat global memory consolidation")?
            .heartbeat_global_phase2_job(ownership_token, lease_seconds)
            .await
    }

    pub async fn mark_global_phase2_job_succeeded(
        &self,
        ownership_token: &str,
        completed_watermark: i64,
        selected_outputs: &[Stage1Output],
    ) -> anyhow::Result<bool> {
        self.sqlite("complete global memory consolidation")?
            .mark_global_phase2_job_succeeded(
                ownership_token,
                completed_watermark,
                selected_outputs,
            )
            .await
    }

    pub async fn mark_global_phase2_job_failed(
        &self,
        ownership_token: &str,
        failure_reason: &str,
        retry_delay_seconds: i64,
    ) -> anyhow::Result<bool> {
        self.sqlite("fail global memory consolidation")?
            .mark_global_phase2_job_failed(ownership_token, failure_reason, retry_delay_seconds)
            .await
    }

    pub async fn mark_global_phase2_job_failed_if_unowned(
        &self,
        ownership_token: &str,
        failure_reason: &str,
        retry_delay_seconds: i64,
    ) -> anyhow::Result<bool> {
        self.sqlite("recover failed global memory consolidation")?
            .mark_global_phase2_job_failed_if_unowned(
                ownership_token,
                failure_reason,
                retry_delay_seconds,
            )
            .await
    }

    #[cfg(test)]
    pub(crate) fn sqlite_pool_for_tests(&self) -> &SqlitePool {
        match &self.backend {
            MemoryStoreBackend::Postgres(_) => {
                panic!("SQLite memory pool requested for a PostgreSQL memory store")
            }
            MemoryStoreBackend::Sqlite(store) => store.pool_for_tests(),
        }
    }
}
