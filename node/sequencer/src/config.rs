use smart_config::{DescribeConfig, DeserializeConfig};
use std::{path::PathBuf, time::Duration};

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct RpcConfig {
    /// JSON-RPC address to listen on. Only http is currently supported.
    #[config(default_t = "127.0.0.1:3050".into())]
    pub address: String,

    /// Max size of a transaction to be accepted by API
    #[config(default_t = 128000)]
    pub max_tx_size_bytes: usize,

    /// Maximal gap between the nonce of the last executed block and the nonce being submitted
    #[config(default_t = 1000)]
    pub max_nonce_ahead: u32,

    /// Gas limit of transactions executed via eth_call
    #[config(default_t = 10000000)]
    pub eth_call_gas: usize,

    /// Number of concurrent API connections (passed to jsonrpsee, default value there is 128)
    #[config(default_t = 1000)]
    pub max_connections: u32,
}


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

    #[config(default_t = 3312)]
    pub prometheus_exporter_port: u16,

    /// Max number of transactions in a block.
    #[config(default_t = 1000)]
    pub max_transactions_in_block: usize
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct BatcherConfig {

    /// Whether to run the batcher (prover input generator) or not.
    /// As it relies on in-memory tree, blockchain will need to replay all blocks on every restart
    #[config(default_t = false)]
    pub component_enabled: bool,

    /// Whether to enable debug output in RiscV binary.
    /// Also known as app.bin vs app_logging_enabled.bin
    #[config(default_t = false)]
    pub logging_enabled: bool,

}
