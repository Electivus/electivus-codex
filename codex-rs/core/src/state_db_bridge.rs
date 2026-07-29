use codex_rollout::state_db as rollout_state_db;
pub use codex_rollout::state_db::StateDbHandle;

use crate::config::Config;

pub async fn init_state_db(config: &Config) -> Option<StateDbHandle> {
    rollout_state_db::init(config).await
}

/// Initializes the selected Runtime State Backend and preserves startup errors.
pub async fn try_init_state_db(config: &Config) -> anyhow::Result<StateDbHandle> {
    rollout_state_db::try_init(config).await
}
