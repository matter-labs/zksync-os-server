use smart_config::{DescribeConfig, DeserializeConfig};
use std::path::PathBuf;

/// Configuration for state storage management
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
pub struct StateConfig {
    /// Min number of blocks to retain in memory
    /// it defines the blocks for which the node can handle API requests
    /// older blocks will be compacted into RocksDb - and thus unavailable for eth_call
    // #[config(default_t = 512)]
    pub blocks_to_retain_in_memory: usize,
    
    /// Path to the RocksDB directory for state storage
    // #[config(default_t = "./db/state".into())]
    pub rocks_db_path: PathBuf,
}