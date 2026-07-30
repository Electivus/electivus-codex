use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;
use uuid::Uuid;

use super::RepairSweepLimits;
use super::list_threads_with_limits;
use crate::ListThreadsParams;
use crate::LocalThreadStoreConfig;
use crate::SortDirection;
use crate::ThreadLocationFilter;
use crate::ThreadPage;
use crate::ThreadSortKey;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::local::LocalThreadStore;
use crate::local::test_support::test_config;
use crate::local::test_support::write_session_file_with;

const PROJECT_IDENTITY: &str = "example.com/acme/project";

struct Harness {
    home: TempDir,
    cwd: PathBuf,
    config: LocalThreadStoreConfig,
    runtime: Option<codex_rollout::StateDbHandle>,
    store: LocalThreadStore,
}

impl Harness {
    fn filesystem() -> Self {
        let home = TempDir::new().expect("temp dir");
        let cwd = home.path().join("project");
        let config = test_config(home.path());
        let store = LocalThreadStore::new(config.clone(), /*state_db*/ None);
        Self {
            home,
            cwd,
            config,
            runtime: None,
            store,
        }
    }

    async fn state() -> Self {
        let mut harness = Self::filesystem();
        let runtime = codex_state::StateRuntime::init(
            harness.config.sqlite.clone(),
            harness.config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        runtime
            .mark_backfill_complete(/*last_watermark*/ None)
            .await
            .expect("backfill should be complete");
        harness.store = LocalThreadStore::new(harness.config.clone(), Some(runtime.clone()));
        harness.runtime = Some(runtime);
        harness
    }

    fn runtime(&self) -> &codex_rollout::StateDbHandle {
        self.runtime.as_ref().expect("state harness")
    }

    fn params(&self) -> ListThreadsParams {
        ListThreadsParams {
            page_size: 10,
            cursor: None,
            sort_key: ThreadSortKey::CreatedAt,
            sort_direction: SortDirection::Desc,
            allowed_sources: vec![SessionSource::Cli],
            model_providers: Some(vec!["test-provider".to_string()]),
            location_filter: ThreadLocationFilter::ProjectSessionScope {
                cwd: self.cwd.clone(),
                repository_identity: PROJECT_IDENTITY.to_string(),
            },
            is_pinned: None,
            archived: false,
            search_term: None,
            relation_filter: None,
            use_state_db_only: false,
        }
    }

    fn write(&self, cwd: &Path, timestamp: &str, uuid: Uuid, message: &str) -> PathBuf {
        self.write_in(
            cwd,
            self.home.path().join("sessions/2025/01/03"),
            timestamp,
            uuid,
            message,
        )
    }

    fn write_exact(&self, timestamp: &str, uuid: Uuid, message: &str) -> PathBuf {
        self.write(&self.cwd, timestamp, uuid, message)
    }

    fn write_archived(&self, timestamp: &str, uuid: Uuid) {
        self.write_in(
            &self.cwd,
            self.home
                .path()
                .join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR),
            timestamp,
            uuid,
            "archived preview",
        );
    }

    fn write_in(
        &self,
        cwd: &Path,
        directory: PathBuf,
        timestamp: &str,
        uuid: Uuid,
        message: &str,
    ) -> PathBuf {
        let path = write_session_file_with(
            cwd,
            directory,
            timestamp,
            uuid,
            message,
            Some("test-provider"),
            ThreadHistoryMode::Legacy,
        )
        .expect("session file");
        let contents = fs::read_to_string(&path).expect("read rollout");
        fs::write(
            &path,
            contents.replace(
                "https://example.com/repo.git",
                "https://example.com/acme/project.git",
            ),
        )
        .expect("write rollout");
        path
    }

    async fn reconcile(&self, path: &Path) {
        codex_rollout::state_db::reconcile_rollout(
            Some(self.runtime().as_ref()),
            path,
            self.config.default_model_provider_id.as_str(),
            /*builder*/ None,
            &[],
            Some(false),
            /*new_thread_memory_mode*/ None,
        )
        .await;
    }

    async fn bounded_list(
        &self,
        scan_budget: usize,
    ) -> Result<codex_rollout::ThreadsPage, ThreadStoreError> {
        self.list_with_limits(RepairSweepLimits {
            scan_budget,
            rollout_source_byte_budget: 32 * 1024 * 1024,
            total_source_byte_budget: 256 * 1024 * 1024,
        })
        .await
    }

    async fn list_with_limits(
        &self,
        limits: RepairSweepLimits,
    ) -> Result<codex_rollout::ThreadsPage, ThreadStoreError> {
        self.list_with_state_and_limits(self.runtime.clone(), limits)
            .await
    }

    async fn list_with_state_and_limits(
        &self,
        state_db: Option<codex_rollout::StateDbHandle>,
        limits: RepairSweepLimits,
    ) -> Result<codex_rollout::ThreadsPage, ThreadStoreError> {
        let rollout_config = codex_rollout::RolloutConfig {
            codex_home: self.config.codex_home.clone(),
            sqlite: self.config.sqlite.clone(),
            cwd: self.config.codex_home.clone(),
            model_provider_id: self.config.default_model_provider_id.clone(),
            generate_memories: false,
        };
        list_threads_with_limits(
            state_db,
            &rollout_config,
            self.config.default_model_provider_id.as_str(),
            &self.params(),
            /*cursor*/ None,
            codex_rollout::ThreadSortKey::CreatedAt,
            codex_rollout::SortDirection::Desc,
            limits,
        )
        .await
    }
}

fn id(uuid: Uuid) -> ThreadId {
    ThreadId::from_string(&uuid.to_string()).expect("valid thread id")
}

fn ids(page: &ThreadPage) -> Vec<ThreadId> {
    page.items.iter().map(|item| item.thread_id).collect()
}

#[tokio::test]
async fn sparse_search_repairs_an_older_exact_cwd_match_before_querying_the_db() {
    let h = Harness::state().await;
    let newer = Uuid::from_u128(201);
    let older = Uuid::from_u128(202);
    h.write_exact("2025-01-03T15-00-00", newer, "ordinary preview");
    h.write_exact("2025-01-03T13-00-00", older, "needle preview");
    let mut params = h.params();
    params.page_size = 1;
    params.search_term = Some("needle".to_string());

    let page = h.store.list_threads(params).await.expect("project listing");

    assert_eq!(ids(&page), vec![id(older)]);
    assert!(
        h.runtime()
            .get_thread(id(newer))
            .await
            .expect("newer state lookup")
            .is_some(),
        "the sweep must reconcile non-matches before DB search filtering"
    );
}

#[tokio::test]
async fn invalid_rollout_fails_project_repair_instead_of_returning_a_db_page() {
    let h = Harness::state().await;
    let uuid = Uuid::from_u128(250);
    let path = h.write_exact("2025-01-03T15-00-00", uuid, "valid preview");
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open rollout"),
        "not-json"
    )
    .expect("append invalid record");

    let error = h
        .store
        .list_threads(h.params())
        .await
        .expect_err("invalid rollout must fail project repair");

    let ThreadStoreError::Internal { message } = error else {
        panic!("expected internal repair error");
    };
    assert_eq!(message, "project repair could not reconcile a rollout");
    assert!(
        h.runtime()
            .get_thread(id(uuid))
            .await
            .expect("state lookup")
            .is_none()
    );
}

#[tokio::test]
async fn db_only_skips_repair_then_full_sweep_returns_exact_cwd_and_repository_union() {
    let h = Harness::state().await;
    let exact = Uuid::from_u128(301);
    let sibling = Uuid::from_u128(302);
    h.write_exact("2025-01-03T15-00-00", exact, "exact preview");
    let sibling_path = h.write(
        &h.home.path().join("sibling"),
        "2025-01-03T14-00-00",
        sibling,
        "sibling preview",
    );
    h.reconcile(&sibling_path).await;
    let mut params = h.params();
    params.use_state_db_only = true;

    let db_only = h
        .store
        .list_threads(params.clone())
        .await
        .expect("DB-only project listing");
    assert_eq!(ids(&db_only), vec![id(sibling)]);
    assert_eq!(
        h.runtime()
            .get_thread(id(exact))
            .await
            .expect("exact state lookup"),
        None
    );

    params.use_state_db_only = false;
    let repaired = h
        .store
        .list_threads(params)
        .await
        .expect("repaired project listing");
    assert_eq!(ids(&repaired), vec![id(exact), id(sibling)]);
    assert_eq!(repaired.next_cursor, None);
}

#[tokio::test]
async fn missing_state_db_falls_back_to_exact_cwd_without_repository_union() {
    let h = Harness::filesystem();
    let exact = Uuid::from_u128(401);
    h.write_exact("2025-01-03T15-00-00", exact, "exact preview");
    h.write(
        &h.home.path().join("sibling"),
        "2025-01-03T14-00-00",
        Uuid::from_u128(402),
        "sibling preview",
    );

    let page = h
        .store
        .list_threads(h.params())
        .await
        .expect("filesystem fallback");

    assert_eq!(ids(&page), vec![id(exact)]);
}

#[tokio::test]
async fn exact_cwd_fallback_fails_when_its_file_scan_budget_is_incomplete() {
    let h = Harness::filesystem();
    for (uuid, timestamp) in [
        (Uuid::from_u128(451), "2025-01-03T15-00-00"),
        (Uuid::from_u128(452), "2025-01-03T14-00-00"),
    ] {
        h.write_exact(timestamp, uuid, "fallback preview");
    }

    let error = h
        .list_with_limits(RepairSweepLimits {
            scan_budget: 1,
            rollout_source_byte_budget: 1024,
            total_source_byte_budget: 2048,
        })
        .await
        .expect_err("incomplete fallback must fail");

    let ThreadStoreError::Internal { message } = error else {
        panic!("expected fallback budget error");
    };
    assert_eq!(
        message,
        "project repair scan budget exceeded: scanned 2 files with budget 1"
    );
}

#[tokio::test]
async fn failed_project_query_falls_back_only_for_desc_without_search_or_db_filters() {
    let h = Harness::filesystem();
    let uuid = Uuid::from_u128(475);
    h.write_exact("2025-01-03T15-00-00", uuid, "fallback preview");
    let other_home = TempDir::new().expect("other temp dir");
    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(other_home.path().abs()),
        h.config.default_model_provider_id.clone(),
    )
    .await
    .expect("mismatched state db");
    let store = LocalThreadStore::new(h.config.clone(), Some(runtime.clone()));

    let page = store
        .list_threads(h.params())
        .await
        .expect("bounded exact-cwd fallback");
    assert_eq!(ids(&page), vec![id(uuid)]);

    let mut pinned = h.params();
    pinned.is_pinned = Some(true);
    assert!(store.list_threads(pinned).await.is_err());

    let mut ascending = h.params();
    ascending.sort_direction = SortDirection::Asc;
    assert!(store.list_threads(ascending).await.is_err());

    let mut search = h.params();
    search.search_term = Some("fallback".to_string());
    assert!(store.list_threads(search).await.is_err());

    let error = h
        .list_with_state_and_limits(
            Some(runtime),
            RepairSweepLimits {
                scan_budget: 1,
                rollout_source_byte_budget: 1024 * 1024,
                total_source_byte_budget: 2 * 1024 * 1024,
            },
        )
        .await
        .expect_err("sweep and fallback must share the file budget");
    let ThreadStoreError::Internal { message } = error else {
        panic!("expected cumulative file-budget error");
    };
    assert_eq!(
        message,
        "project repair scan budget exceeded: scanned 2 files with budget 1"
    );
}

#[tokio::test]
async fn db_owned_filters_skip_project_repair() {
    let h = Harness::state().await;
    let uuid = Uuid::from_u128(490);
    let path = h.write_exact("2025-01-03T15-00-00", uuid, "valid preview");
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open rollout"),
        "not-json"
    )
    .expect("append invalid record");
    let mut params = h.params();
    params.is_pinned = Some(true);

    let page = h
        .store
        .list_threads(params)
        .await
        .expect("pin filter should query the DB without repair");

    assert!(page.items.is_empty());
    assert!(h.runtime().get_thread(id(uuid)).await.unwrap().is_none());
}

#[tokio::test]
async fn archived_project_listing_repairs_the_archived_root() {
    let h = Harness::state().await;
    let archived = Uuid::from_u128(501);
    h.write_archived("2025-01-03T15-00-00", archived);
    let mut params = h.params();
    params.archived = true;

    let page = h
        .store
        .list_threads(params)
        .await
        .expect("archived project listing");

    assert_eq!(ids(&page), vec![id(archived)]);
}

#[tokio::test]
async fn repair_sweep_scans_created_at_ties_once_before_the_db_query() {
    let h = Harness::state().await;
    let lower = Uuid::from_u128(601);
    let higher = Uuid::from_u128(602);
    for uuid in [lower, higher] {
        h.write_exact("2025-01-03T15-00-00", uuid, "tie preview");
    }

    let page = h.bounded_list(/*scan_budget*/ 2).await.expect("tied sweep");

    assert_eq!(
        page.items
            .iter()
            .filter_map(|item| item.thread_id)
            .collect::<Vec<_>>(),
        vec![id(higher), id(lower)]
    );
}

#[tokio::test]
async fn repair_budget_exhaustion_returns_an_error_instead_of_a_db_page() {
    let h = Harness::state().await;
    for (uuid, timestamp) in [
        (Uuid::from_u128(701), "2025-01-03T15-00-00"),
        (Uuid::from_u128(702), "2025-01-03T14-00-00"),
    ] {
        h.write_exact(timestamp, uuid, "budget preview");
    }

    let error = h
        .bounded_list(/*scan_budget*/ 1)
        .await
        .expect_err("bounded repair must not return a partial DB page");

    let ThreadStoreError::Internal { message } = error else {
        panic!("expected internal repair-budget error");
    };
    assert_eq!(
        message,
        "project repair scan budget exceeded: scanned 2 files with budget 1"
    );
}

#[tokio::test]
async fn rollout_source_byte_limit_fails_before_the_project_query() {
    let h = Harness::state().await;
    let uuid = Uuid::from_u128(750);
    let path = h.write_exact("2025-01-03T15-00-00", uuid, "oversized preview");
    let source_bytes = fs::metadata(path).expect("rollout metadata").len();

    let error = h
        .list_with_limits(RepairSweepLimits {
            scan_budget: 100,
            rollout_source_byte_budget: source_bytes - 1,
            total_source_byte_budget: source_bytes * 2,
        })
        .await
        .expect_err("oversized rollout must fail repair");

    assert!(matches!(error, ThreadStoreError::Internal { .. }));
    assert!(h.runtime().get_thread(id(uuid)).await.unwrap().is_none());
}

#[tokio::test]
async fn cumulative_source_byte_limit_fails_before_the_project_query() {
    let h = Harness::state().await;
    let first = Uuid::from_u128(751);
    let second = Uuid::from_u128(752);
    let path = h.write_exact("2025-01-03T15-00-00", first, "same preview");
    h.write_exact("2025-01-03T14-00-00", second, "same preview");
    let one_rollout_bytes = fs::metadata(path).expect("rollout metadata").len();

    let error = h
        .list_with_limits(RepairSweepLimits {
            scan_budget: 100,
            rollout_source_byte_budget: one_rollout_bytes * 2,
            total_source_byte_budget: one_rollout_bytes,
        })
        .await
        .expect_err("request byte budget must fail repair");

    assert!(matches!(error, ThreadStoreError::Internal { .. }));
}

#[tokio::test]
async fn repair_refreshes_legacy_identity_but_preserves_explicit_reclassification() {
    let h = Harness::state().await;
    let uuid = Uuid::from_u128(801);
    let path = h.write_exact("2025-01-03T15-00-00", uuid, "identity preview");
    h.reconcile(&path).await;
    let id = id(uuid);
    let mut stale = h
        .runtime()
        .get_thread(id)
        .await
        .expect("state lookup")
        .expect("reconciled metadata");
    stale.cwd = h.home.path().join("stale");
    stale.git_origin_url = Some("https://example.com/acme/stale.git".to_string());
    h.runtime()
        .upsert_thread(&stale)
        .await
        .expect("seed stale metadata");

    h.store
        .list_threads(h.params())
        .await
        .expect("repair stale metadata");
    let refreshed = h
        .runtime()
        .get_thread(id)
        .await
        .expect("state lookup")
        .expect("refreshed metadata");
    assert_eq!(
        (
            refreshed.cwd,
            refreshed.git_origin_url.as_deref(),
            refreshed.repository_identity.as_deref(),
        ),
        (
            h.cwd.clone(),
            Some("https://example.com/acme/project.git"),
            Some(PROJECT_IDENTITY),
        )
    );

    h.runtime()
        .update_thread_git_info(
            id,
            /*git_sha*/ None,
            /*git_branch*/ None,
            Some(Some("https://example.com/acme/reclassified.git")),
        )
        .await
        .expect("explicit reclassification");
    h.store
        .list_threads(h.params())
        .await
        .expect("repair explicit metadata");
    let preserved = h
        .runtime()
        .get_thread(id)
        .await
        .expect("state lookup")
        .expect("preserved metadata");
    assert_eq!(
        (
            preserved.git_origin_url.as_deref(),
            preserved.repository_identity.as_deref(),
        ),
        (
            Some("https://example.com/acme/reclassified.git"),
            Some("example.com/acme/reclassified"),
        )
    );
}
