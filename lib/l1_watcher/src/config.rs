use std::time::Duration;

/// Configuration of L1 watcher.
#[derive(Clone, Debug)]
pub struct L1WatcherConfig {
    /// Max number of L1 blocks to be processed at a time.
    pub max_blocks_to_process: u64,

    /// Number of latest L1 blocks to leave unprocessed in order to reduce reorg risk.
    pub confirmations: u64,

    /// How often to poll L1 for new priority requests.
    pub poll_interval: Duration,

    /// Max wall-clock time a single `poll()` invocation may take. Exceeding it indicates a
    /// silent hang (e.g. an RPC call that never returns on a half-dead TCP connection); the
    /// watcher task panics so its critical-task supervisor restarts it with fresh state.
    pub poll_iteration_timeout: Duration,

    /// Max wall-clock time `L1TxWatcher` will wait for a freshly observed L1 priority op to
    /// become visible on the settlement layer before erroring out (which panics & recycles the
    /// task). Caps the otherwise-unbounded poll loop that re-queries the SL every 10s.
    pub sl_wait_timeout: Duration,
}
