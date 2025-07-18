use smart_config::metadata::TimeUnit;
use smart_config::{DescribeConfig, DeserializeConfig};
use std::{path::PathBuf, time::Duration};

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct RpcConfig {
    /// JSON-RPC address to listen on. Only http is currently supported.
    #[config(default_t = "0.0.0.0:3050".into())]
    pub address: String,

    /// Chain ID of the chain node operates on.
    #[config(default_t = 270)]
    pub chain_id: u64,

    /// Gas limit of transactions executed via eth_call
    #[config(default_t = 10000000)]
    pub eth_call_gas: usize,

    /// Number of concurrent API connections (passed to jsonrpsee, default value there is 128)
    #[config(default_t = 1000)]
    pub max_connections: u32,

    /// Maximum number of blocks that could be scanned per filter
    #[config(default_t = 100_000)]
    pub max_blocks_per_filter: u64,

    /// Maximum number of logs that can be returned in a response
    #[config(default_t = 20_000)]
    pub max_logs_per_response: usize,

    /// Duration since the last filter poll, after which the filter is considered stale
    #[config(default_t = 15 * TimeUnit::Minutes)]
    pub stale_filter_ttl: Duration,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct MempoolConfig {
    /// Max input size of a transaction to be accepted by mempool
    #[config(default_t = 128 * 1024 * 1024)]
    pub max_tx_input_bytes: usize,
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

    /// Max number of transactions in a block.
    #[config(default_t = 1000)]
    pub max_transactions_in_block: usize,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct BatcherConfig {
    /// Whether to run the batcher (prover input generator) or not.
    /// As it relies on in-memory tree, blockchain will need to replay all blocks on every restart.
    #[config(default_t = true)]
    pub component_enabled: bool,

    /// Whether to enable debug output in RiscV binary.
    /// Also known as app.bin vs app_logging_enabled.bin
    #[config(default_t = false)]
    pub logging_enabled: bool,

    /// How many blocks should be worked on at once.
    /// The batcher will wait for block N to finish before starting block N + maximum_in_flight_blocks.
    #[config(default_t = 10)]
    pub maximum_in_flight_blocks: usize,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct ProverApiConfig {
    /// Whether to run the prover api or not.
    /// If enabled, prover jobs must be consumed - otherwise it will apply back-pressure upstream.
    #[config(default_t = false)]
    pub component_enabled: bool,

    /// Whether to enable debug output in RiscV binary.
    /// Also known as app.bin vs app_logging_enabled.bin
    #[config(default_t = Duration::from_secs(180))]
    pub job_timeout: Duration,

    /// Prover API address to listen on.
    #[config(default_t = "0.0.0.0:3124".into())]
    pub address: String,

    /// Upper bound on the number of FRI blocks whose **prover inputs** are still
    /// retained in memory while a proof is outstanding.
    ///
    /// * When the threshold is reached, the batching stage applies back-pressure,
    ///   which propagates up to block production.
    /// * Each unproved block holds its entire prover-input blob in RAM, so this
    ///   value must remain bounded.
    ///
    #[config(default_t = 1000)]
    pub max_unproved_blocks: usize,
}
