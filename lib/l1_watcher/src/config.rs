use alloy::primitives::Address;
use smart_config::Serde;
use smart_config::metadata::TimeUnit;
use smart_config::{DescribeConfig, DeserializeConfig};
use std::path::PathBuf;
use std::time::Duration;

/// Configuration of L1 watcher.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct L1WatcherConfig {
    /// Path to the root directory for RocksDB.
    #[config(default_t = "./db/node1".into())]
    pub rocks_db_path: PathBuf,

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
    #[config(with = Serde![str], default_t = "0x4b37536b9824c4a4cf3d15362135e346adb7cb9c".parse().unwrap())]
    pub bridgehub_address: Address,
}
