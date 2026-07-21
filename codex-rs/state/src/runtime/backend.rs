use crate::SqliteConfig;

/// Configuration selecting the Runtime State Backend used by [`super::StateRuntime`].
///
/// SQLite is the only selectable backend until the PostgreSQL implementation
/// satisfies the complete Runtime State Contract.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeStateBackendConfig {
    Sqlite(SqliteConfig),
}
