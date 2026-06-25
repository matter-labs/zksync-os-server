use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event};
use alloy::rpc::types::Log;
use zksync_os_contract_interface::IExecutor::BlocksRevert;
use zksync_os_contract_interface::ZkChain;
use zksync_os_provider::NodeProvider;

/// Watches settlement-layer `BlocksRevert` events and crashes the node when the main node reverts
/// committed L1 batches.
pub struct L1RevertWatcher {
    /// SL block number used to initialize finality at startup. Reverts at or below this block are
    /// already reflected in the startup frontier and must be ignored.
    startup_sl_block: u64,
}

impl L1RevertWatcher {
    pub fn create_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<NodeProvider>,
        startup_sl_block: u64,
    ) -> L1Watcher<L1RevertWatcher> {
        tracing::info!(
            startup_sl_block,
            zk_chain_address = ?zk_chain.address(),
            "initializing L1 revert watcher"
        );
        let this = Self { startup_sl_block };
        // Process forward from the startup SL block; reverts in earlier blocks are already accounted for.
        L1Watcher::new_finalized(
            config,
            zk_chain.provider().clone(),
            (*zk_chain.address()).into(),
            startup_sl_block + 1,
            None,
            this,
        )
    }
}

/// Returns true if the revert event happened after startup and therefore requires a restart.
fn should_restart_for_revert(startup_sl_block: u64, log_block_number: Option<u64>) -> bool {
    log_block_number.is_some_and(|log_block_number| log_block_number > startup_sl_block)
}

#[async_trait::async_trait]
impl ProcessL1Event for L1RevertWatcher {
    const NAME: &'static str = "blocks_revert";

    type SolEvent = BlocksRevert;
    type WatchedEvent = BlocksRevert;

    async fn process_event(
        &mut self,
        _provider: &NodeProvider,
        revert: BlocksRevert,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        let total_batches_committed = revert.totalBatchesCommitted.to::<u64>();
        if should_restart_for_revert(self.startup_sl_block, log.block_number) {
            tracing::error!(
                total_batches_committed,
                total_batches_verified = %revert.totalBatchesVerified,
                total_batches_executed = %revert.totalBatchesExecuted,
                log_block_number = ?log.block_number,
                "detected L1 batch revert on the settlement layer; restarting to re-sync from the main node",
            );
            return Err(L1WatcherError::L1Reverted(total_batches_committed));
        }
        tracing::warn!(
            total_batches_committed,
            log_block_number = ?log.block_number,
            startup_sl_block = self.startup_sl_block,
            "skipping historical L1 batch revert at or below startup SL block; already reflected in startup frontier",
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::should_restart_for_revert;

    #[test]
    fn restarts_for_revert_after_startup_block() {
        assert!(should_restart_for_revert(100, Some(101)));
    }

    #[test]
    fn does_not_restart_for_revert_at_startup_block() {
        assert!(!should_restart_for_revert(100, Some(100)));
    }

    #[test]
    fn does_not_restart_for_revert_below_startup_block() {
        assert!(!should_restart_for_revert(100, Some(99)));
    }

    #[test]
    fn does_not_restart_when_log_has_no_block_number() {
        assert!(!should_restart_for_revert(100, None));
    }
}
