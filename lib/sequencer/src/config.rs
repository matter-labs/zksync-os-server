use smart_config::{DescribeConfig, DeserializeConfig};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct SequencerConfig {
    /// Min number of blocks to retain in memory
    /// it defines the blocks for which the node can handle API requests
    /// older blocks will be compacted into RocksDb - and thus unavailable for `eth_call`.
    ///
    /// Currently, it affects both the storage logs (see `state` crate)
    /// and repositories (see `repositories` package in this crate)
    #[config(default_t = 512)]
    pub blocks_to_retain_in_memory: usize,

    /// Path to the directory for persistence (eg RocksDB) - will contain both state and repositories' DBs
    #[config(default_t = "./db/node1".into())]
    pub rocks_db_path: PathBuf,

    /// Defines the block time for the sequencer.
    #[config(default_t = Duration::from_millis(100))]
    pub block_time: Duration,

    /// Max number of transactions in a block.
    #[config(default_t = 1000)]
    pub max_transactions_in_block: usize,

    /// Path to the directory where block dumps for unexpected failures will be saved.
    #[config(default_t = "./block_dumps".into())]
    pub block_dump_path: PathBuf,

    /// If set to true, the server will replay all blocks starting from genesis.
    /// Useful when there are inconsistencies in saved block numbers.
    #[config(default_t = false)]
    pub replay_all_blocks_unsafe: bool,

    /// Prometheus address to listen on.
    #[config(default_t = 3312)]
    pub prometheus_port: u16,

    /// Where to serve block replays
    #[config(default_t = "0.0.0.0:3053".into())]
    pub block_replay_server_address: String,

    /// Where to download replays instead of actually running blocks.
    /// Setting this makes the node into an external node.
    #[config(default_t = None)]
    pub block_replay_download_address: Option<String>,

    /// If set - initialize the configs based off the values from the yaml files from that directory.
    pub zkstack_cli_config_dir: Option<String>,
}
