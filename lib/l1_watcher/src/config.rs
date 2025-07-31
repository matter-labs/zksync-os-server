use alloy::primitives::Address;
use smart_config::Serde;
use smart_config::metadata::TimeUnit;
use smart_config::{DescribeConfig, DeserializeConfig};
use std::time::Duration;

/// Configuration of L1 watcher.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct L1WatcherConfig {
    /// L1's JSON RPC API.
    #[config(default_t = "ws://localhost:8545".into())]
    pub l1_api_url: String,

    /// Max number of L1 blocks to be processed at a time.
    #[config(default_t = 100)]
    pub max_blocks_to_process: u64,

    /// How often to poll L1 for new priority requests.
    #[config(default_t = 100 * TimeUnit::Millis)]
    pub poll_interval: Duration,

    /// L1 address of `Bridgehub` contract. This is an entrypoint into L1 discoverability so most
    /// other contracts should be discoverable through it.
    // TODO: Pre-configured value, to be removed
    #[config(with = Serde![str], default_t = "0x2bb295fe80bfcc2a9336402a5ad5ac099784b44f".parse().unwrap())]
    pub bridgehub_address: Address,
}
