/// Maximum UTF-8 byte length of Repository identity rendered into model context.
pub(crate) const MAX_REPOSITORY_IDENTITY_BYTES: usize = 1024;

/// Credential-free canonical identity used by the Memories subsystem.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepositoryIdentity(String);

impl RepositoryIdentity {
    pub(crate) fn from_git_origin_url(git_origin_url: Option<&str>) -> Option<Self> {
        let repository = git_origin_url.and_then(codex_git_utils::canonicalize_git_remote_url)?;
        if repository.len() > MAX_REPOSITORY_IDENTITY_BYTES
            || repository
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
        {
            return None;
        }
        Some(Self(repository))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}
