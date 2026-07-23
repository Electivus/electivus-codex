use crate::PostgresNamespaceConfig;
use crate::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;

/// Configuration selecting the Runtime State Backend used by [`super::StateRuntime`].
///
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeStateBackendConfig {
    Sqlite(SqliteConfig),
    Postgresql {
        codex_home: AbsolutePathBuf,
        namespace: PostgresNamespaceConfig,
    },
}

impl RuntimeStateBackendConfig {
    /// Returns whether PostgreSQL is the selected Runtime State Backend.
    pub fn is_postgresql(&self) -> bool {
        matches!(self, Self::Postgresql { .. })
    }
}
