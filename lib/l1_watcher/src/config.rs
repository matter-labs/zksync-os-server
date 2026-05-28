use std::time::Duration;

/// Configuration of L1 watcher.
#[derive(Clone, Debug)]
pub struct L1WatcherConfig {
    /// Max number of L1 blocks to be processed at a time.
    pub max_blocks_to_process: u64,

    /// Number of latest L1 blocks to leave unprocessed in order to reduce reorg risk.
    pub confirmations: u64,

    /// How often to poll L1 for the latest block.
    /// Also used for polling latest finalized block, unless overriden by `finalized_poll_interval`
    pub poll_interval: Duration,

    /// How often to poll L1 for the latest finalized block.
    /// If unset, regular `poll_interval` will be used.
    /// Note: Finalization advances at epoch boundaries. Which is every ~6.4 minutes on L1.
    pub finalized_poll_interval: Option<Duration>,
}
