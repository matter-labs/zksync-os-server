use std::time::Duration;

/// Configuration of L1 watcher.
#[derive(Clone, Debug)]
pub struct L1WatcherConfig {
    /// Max number of L1 blocks to be processed at a time.
    pub max_blocks_to_process: u64,

    /// Number of latest L1 blocks to leave unprocessed in order to reduce reorg risk.
    pub confirmations: u64,

    /// Ingest block-content events (priority transactions, protocol upgrades) only once
    /// their L1 block is finalized, instead of `confirmations` blocks behind the tip.
    /// Consensus-enabled nodes require this: these events end up inside finalized L2
    /// blocks, which cannot be rolled back if the L1 event reorgs away. Set by the node
    /// wiring based on the consensus mode, not by operators.
    pub finalized_ingestion: bool,

    /// How often to poll L1 for the latest block.
    pub poll_interval: Duration,

    /// How often to poll L1 for the latest finalized block.
    /// Note: Finalization advances at epoch boundaries. Which is every ~6.4 minutes on L1.
    pub finalized_poll_interval: Duration,

    /// Number of recent blocks retained in the shared logs cache.
    pub logs_cache_capacity: usize,
}
