use std::path::PathBuf;

use codex_app_server_protocol::ThreadListCwdFilter;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadSortKey;
use codex_app_server_protocol::ThreadSourceKind;
use codex_utils_path_uri::LegacyAppPathString;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionFilterMode {
    Project,
    Cwd,
    All,
}

impl SessionFilterMode {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::Cwd => "Cwd",
            Self::All => "All",
        }
    }

    fn changed(self, direction: ScopeChangeDirection, has_current_cwd: bool) -> Self {
        if !has_current_cwd {
            return Self::All;
        }
        match (self, direction) {
            (Self::Project, ScopeChangeDirection::Previous) => Self::All,
            (Self::Project, ScopeChangeDirection::Next) => Self::Cwd,
            (Self::Cwd, ScopeChangeDirection::Previous) => Self::Project,
            (Self::Cwd, ScopeChangeDirection::Next) => Self::All,
            (Self::All, ScopeChangeDirection::Previous) => Self::Cwd,
            (Self::All, ScopeChangeDirection::Next) => Self::Project,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ScopeChangeDirection {
    Previous,
    Next,
}

pub(super) struct SessionScope {
    mode: SessionFilterMode,
    current_cwd: Option<PathBuf>,
}

impl SessionScope {
    pub(super) fn project(current_cwd: Option<PathBuf>) -> Self {
        let mode = if current_cwd.is_some() {
            SessionFilterMode::Project
        } else {
            SessionFilterMode::All
        };
        Self { mode, current_cwd }
    }

    pub(super) fn all(current_cwd: Option<PathBuf>) -> Self {
        Self {
            mode: SessionFilterMode::All,
            current_cwd,
        }
    }

    pub(super) fn mode(&self) -> SessionFilterMode {
        self.mode
    }

    pub(super) fn has_current_cwd(&self) -> bool {
        self.current_cwd.is_some()
    }

    pub(super) fn change(&mut self, direction: ScopeChangeDirection) -> bool {
        let next_mode = self.mode.changed(direction, self.current_cwd.is_some());
        if self.mode == next_mode {
            return false;
        }
        self.mode = next_mode;
        true
    }

    pub(super) fn location_filter(&self) -> SessionLocationFilter {
        match (self.mode, self.current_cwd.as_ref()) {
            (SessionFilterMode::Project, Some(current_cwd)) => {
                SessionLocationFilter::Project(current_cwd.clone())
            }
            (SessionFilterMode::Cwd, Some(current_cwd)) => {
                SessionLocationFilter::Cwd(current_cwd.clone())
            }
            (SessionFilterMode::Project | SessionFilterMode::Cwd, None)
            | (SessionFilterMode::All, _) => SessionLocationFilter::All,
        }
    }

    pub(super) fn shows_recorded_working_directory(&self) -> bool {
        self.mode != SessionFilterMode::Cwd
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SessionLocationFilter {
    Project(PathBuf),
    Cwd(PathBuf),
    All,
}

pub(super) struct ThreadListQuery {
    pub(super) cursor: Option<String>,
    pub(super) limit: u32,
    pub(super) sort_key: ThreadSortKey,
    pub(super) model_providers: Option<Vec<String>>,
    pub(super) source_kinds: Vec<ThreadSourceKind>,
    pub(super) location_filter: SessionLocationFilter,
}

impl ThreadListQuery {
    pub(super) fn into_params(self) -> ThreadListParams {
        let (cwd, project_cwd) = match self.location_filter {
            SessionLocationFilter::Project(project_cwd) => (
                None,
                Some(LegacyAppPathString::from_path(project_cwd.as_path())),
            ),
            SessionLocationFilter::Cwd(cwd) => (
                Some(ThreadListCwdFilter::One(cwd.to_string_lossy().into_owned())),
                None,
            ),
            SessionLocationFilter::All => (None, None),
        };
        ThreadListParams {
            cursor: self.cursor,
            limit: Some(self.limit),
            sort_key: Some(self.sort_key),
            sort_direction: None,
            model_providers: self.model_providers,
            source_kinds: Some(self.source_kinds),
            archived: Some(false),
            is_pinned: None,
            cwd,
            project_cwd,
            use_state_db_only: false,
            search_term: None,
            parent_thread_id: None,
            ancestor_thread_id: None,
        }
    }
}

#[cfg(test)]
#[path = "resume_picker_scope_tests.rs"]
mod tests;
