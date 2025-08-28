use crate::config::GeneralConfig;
use async_trait::async_trait;
use std::future;
use zksync_os_genesis::Genesis;
use zksync_os_state::StateHandle;
use zksync_os_state_full_diffs::FullDiffsState;

#[async_trait]
pub trait StateInitializer: Sized {
    async fn new(config: &GeneralConfig, genesis: &Genesis) -> Self;

    // default no-op
    async fn compact_periodically_optional(&self) {
        future::pending::<()>().await;
    }
}

#[async_trait]
impl StateInitializer for StateHandle {
    async fn new(config: &GeneralConfig, genesis: &Genesis) -> Self {
        StateHandle::new(
            config.rocks_db_path.clone(),
            config.blocks_to_retain_in_memory,
            genesis,
        )
        .await
    }

    async fn compact_periodically_optional(&self) {
        self.compact_periodically().await;
    }
}

#[async_trait]
impl StateInitializer for FullDiffsState {
    async fn new(config: &GeneralConfig, genesis: &Genesis) -> Self {
        FullDiffsState::new(config.rocks_db_path.clone(), genesis)
            .await
            .expect("Failed to initialize full diffs state")
    }
}
