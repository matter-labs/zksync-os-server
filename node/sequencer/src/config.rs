use smart_config::{DescribeConfig, DeserializeConfig};
use std::{path::PathBuf, time::Duration};
use zksync_os_object_store::ObjectStoreConfig;
pub use zksync_os_rpc::RpcConfig;
/// Configuration for the sequencer node.
/// Includes configurations of all subsystems.
/// Default values are provided for local setup.

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

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct BatcherConfig {
    /// Whether to run the batcher subsystem or not
    #[config(default_t = true)]
    pub subsystem_enabled: bool,

    /// How long to keep a batch open before sealing it.
    #[config(default_t = Duration::from_secs(3))]
    pub batch_timeout: Duration,

    /// Max number of blocks per batch
    #[config(default_t = 5)]
    pub blocks_per_batch_limit: usize,
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
    /// Prover API address to listen on.
    #[config(default_t = "0.0.0.0:3124".into())]
    pub address: String,

    #[config(nest)]
    pub fake_fri_provers: FakeFriProversConfig,

    #[config(nest)]
    /// If this value is set to false but FRI fake provers are enabled,
    /// we'll still use fake SNARK proofs for fake FRI proofs -
    /// however, we won't turn real FRI proofs into fake ones - even on timeout.
    pub fake_snark_provers: FakeSnarkProversConfig,

    /// Timeout after which a prover job is assigned to another Fri Prover Worker.
    #[config(default_t = Duration::from_secs(300))]
    pub job_timeout: Duration,

    /// Max difference between the oldest and newest batch number being proven
    /// If the difference is larger than this, provers will not be assigned new jobs.
    /// We use max range instead of length limit to avoid having one old batch stuck -
    /// otherwise GaplessCommitter's buffer would grow indefinitely.
    #[config(default_t = 20)]
    pub max_assigned_batch_range: usize,

    /// Max number of FRI proofs that will be aggregated to a single SNARK job.
    #[config(default_t = 10)]
    pub max_fris_per_snark: usize,

    #[config(nest, default)]
    pub object_store: ObjectStoreConfig,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct FakeFriProversConfig {
    /// Whether to enable the fake provers pool.
    #[config(default_t = true)]
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
pub struct FakeSnarkProversConfig {
    /// Whether to enable the fake provers pool.
    #[config(default_t = true)]
    pub enabled: bool,

    /// Number of fake provers to run in parallel.
    #[config(default_t = Duration::from_secs(10))]
    pub max_batch_age: Duration,
}

// todo/consider: I'm not sure we need this as an external config.
// maybe we should have an L1Config with minimal set of needed info,
// and recover everything from there.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct GenesisConfig {
    /// Chain ID of the chain node operates on.
    #[config(default_t = 270)]
    pub chain_id: u64,

    /// Path to the file with genesis input.
    #[config(default_t = "./genesis/genesis.json".into())]
    pub genesis_input_path: PathBuf,
}
