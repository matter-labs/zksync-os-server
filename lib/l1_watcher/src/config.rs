use smart_config::metadata::TimeUnit;
use smart_config::{DescribeConfig, DeserializeConfig};
use std::time::Duration;

/// Configuration of L1 watcher.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct L1WatcherConfig {
    /// Max number of L1 blocks to be processed at a time.
    #[config(default_t = 100)]
    pub max_blocks_to_process: u64,

    /// How often to poll L1 for new priority requests.
    #[config(default_t = 100 * TimeUnit::Millis)]
    pub poll_interval: Duration,
}
