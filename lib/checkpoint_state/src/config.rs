use smart_config::{DescribeConfig, DeserializeConfig};
use std::path::PathBuf;

/// Configuration for state storage management
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
pub struct StateConfig {
    /// If set to true, the state storage will be erased on startup
    /// Only use when replaying/starting from genesis
    pub erase_storage_on_start: bool,

    pub checkpoints_to_retain: usize,

    /// Path to the RocksDB directory for state storage
    pub rocks_db_path: PathBuf,
}
