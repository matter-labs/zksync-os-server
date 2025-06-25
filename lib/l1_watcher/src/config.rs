use smart_config::metadata::TimeUnit;
use smart_config::{DescribeConfig, DeserializeConfig};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use zksync_types::Address;

/// Configuration of L1 watcher.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct L1WatcherConfig {
    /// Path to the root directory for RocksDB.
    #[config(default_t = "./db/node1".into())]
    pub rocks_db_path: PathBuf,

    /// L1's JSON RPC API.
    #[config(default_t = "http://localhost:8545".into())]
    pub l1_api_url: String,

    /// L2 chain ID to monitor priority requests for.
    #[config(default_t = 270)]
    pub chain_id: u64,

    /// Max number of L1 blocks to be processed at a time.
    #[config(default_t = 100)]
    pub max_blocks_to_process: u64,

    /// How often to poll L1 for new priority requests.
    #[config(default_t = 100 * TimeUnit::Millis)]
    pub l1_poll_interval: Duration,

    /// L1 address of `Bridgehub` contract. This is an entrypoint into L1 discoverability so most
    /// other contracts should be discoverable through it.
    // TODO: Pre-configured value, to be removed
    #[config(default_t = FromStr::from_str("0x4b37536b9824c4a4cf3d15362135e346adb7cb9c").unwrap())]
    pub bridgehub_address: Address,
}
