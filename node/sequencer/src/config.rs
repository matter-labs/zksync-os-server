use smart_config::metadata::TimeUnit;
use smart_config::{DescribeConfig, DeserializeConfig};
use std::{path::PathBuf, time::Duration};

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct RpcConfig {
    /// JSON-RPC address to listen on. Only http is currently supported.
    #[config(default_t = "0.0.0.0:3050".into())]
    pub address: String,

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
    /// Min number of blocks to retain in memory.
    /// It defines the blocks for which the node can handle API requests.
    /// Older blocks will be compacted into RocksDB - and thus unavailable for `eth_call`.
    ///
    /// Currently, it affects both the storage logs (see `state` crate)
    /// and repositories (see `repositories` package in this crate).
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
    /// Whether to run the batcher subsystem or not
    #[config(default_t = true)]
    pub subsystem_enabled: bool,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct ProverInputGeneratorConfig {
    /// Whether to enable debug output in RiscV binary.
    /// Also known as server_app.bin vs server_app_logging_enabled.bin
    #[config(default_t = false)]
    pub logging_enabled: bool,

    /// How many blocks should be worked on at once.
    /// The batcher will wait for block N to finish before starting block N + maximum_in_flight_blocks.
    #[config(default_t = 16)]
    pub maximum_in_flight_blocks: usize,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct ProverApiConfig {
    #[config(nest)]
    pub fake_provers: FakeProversConfig,

    /// Timeout after which a prover job is assigned to another Prover Worker.
    #[config(default_t = Duration::from_secs(300))]
    pub job_timeout: Duration,

    /// Max difference between the oldest and newest batch number being proven
    /// If the difference is larger than this, provers will not be assigned new jobs.
    /// We use max range instead of length limit to avoid having one old batch stuck -
    /// otherwise GaplessCommitter's buffer would grow indefinitely.
    #[config(default_t = 50)]
    pub max_assigned_batch_range: usize,

    /// Prover API address to listen on.
    #[config(default_t = "0.0.0.0:3124".into())]
    pub address: String,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct FakeProversConfig {
    /// Whether to enable the fake provers pool.
    #[config(default_t = false)]
    pub enabled: bool,

    /// Number of fake provers to run in parallel.
    #[config(default_t = 10)]
    pub workers: usize,

    /// Amount of time it takes to compute a proof for one batch.
    /// todo: Doesn't account for batch size at the moment
    #[config(default_t = Duration::from_millis(2000))]
    pub compute_time: Duration,

    /// Only pick up jobs that are this time old
    /// This gives real provers a head start when picking jobs
    #[config(default_t = Duration::from_millis(3000))]
    pub min_age: Duration,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct GenesisConfig {
    /// Chain ID of the chain node operates on.
    #[config(default_t = 270)]
    pub chain_id: u64,
}
