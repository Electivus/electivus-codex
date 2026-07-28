/// Credential-free canonical identity used by the Memories subsystem.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepositoryIdentity(String);

impl RepositoryIdentity {
    pub(crate) fn from_git_origin_url(git_origin_url: Option<&str>) -> Option<Self> {
        git_origin_url
            .and_then(codex_git_utils::canonicalize_git_remote_url)
            .map(Self)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}
