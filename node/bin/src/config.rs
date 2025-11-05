use crate::command_source::RebuildOptions;
use alloy::consensus::constants::GWEI_TO_WEI;
use alloy::primitives::Address;
use serde::{Deserialize, Serialize};
use smart_config::metadata::TimeUnit;
use smart_config::value::SecretString;
use smart_config::{
    DescribeConfig, DeserializeConfig, Serde,
    de::{Delimited, Optional},
};
use std::{path::PathBuf, time::Duration};
use zksync_os_contract_interface::models::BatchDaInputMode;
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_l1_sender::commands::execute::ExecuteCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_mempool::SubPoolLimit;
use zksync_os_object_store::ObjectStoreConfig;
use zksync_os_observability::LogFormat;
use zksync_os_observability::opentelemetry::OpenTelemetryLevel;

/// Configuration for the sequencer node.
/// Includes configurations of all subsystems.
/// Default values are provided for local setup.
#[derive(Debug)]
pub struct Config {
    pub general_config: GeneralConfig,
    pub genesis_config: GenesisConfig,
    pub rpc_config: RpcConfig,
    pub mempool_config: MempoolConfig,
    pub tx_validator_config: TxValidatorConfig,
    pub sequencer_config: SequencerConfig,
    pub l1_sender_config: L1SenderConfig,
    pub l1_watcher_config: L1WatcherConfig,
    pub batcher_config: BatcherConfig,
    pub prover_input_generator_config: ProverInputGeneratorConfig,
    pub prover_api_config: ProverApiConfig,
    pub status_server_config: StatusServerConfig,
    pub observability_config: ObservabilityConfig,
    pub gas_adjuster_config: GasAdjusterConfig,
}

/// "Umbrella" config for the node.
/// If variable is shared i.e. used by multiple components OR does not belong to any specific component (e.g. `zkstack_cli_config_dir`)
/// then it belongs here.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct GeneralConfig {
    /// L1's JSON RPC API.
    #[config(default_t = "http://localhost:8545".into())]
    pub l1_rpc_url: String,

    /// Min number of blocks to replay on restart
    /// Depending on L1/persistence state, we may need to replay more blocks than this number
    /// In some cases, we need to replay the whole blockchain (e.g. switching state backends) -
    /// in such cases a warning is logged.
    #[config(default_t = 10)]
    pub min_blocks_to_replay: usize,

    /// Force a block number to start replaying from.
    /// For Compacted backend it can either be `0` or `last_compacted_block + 1`.
    /// For FullDiffs backend:
    ///     On EN: can be any historical block number;
    ///     On Main Node: any historical block number up to the last l1 committed one.
    #[config(default_t = None)]
    pub force_starting_block_number: Option<u64>,

    /// Path to the directory for persistence (eg RocksDB) - will contain both state and repositories' DBs
    #[config(default_t = "./db/node1".into())]
    pub rocks_db_path: PathBuf,

    /// State backend to use. When changed, a replay of all blocks may be needed.
    #[config(default_t = StateBackendConfig::FullDiffs)]
    #[config(with = Serde![str])]
    pub state_backend: StateBackendConfig,

    /// Min number of blocks to retain in memory
    /// it defines the blocks for which the node can handle API requests
    /// older blocks will be compacted into RocksDb - and thus unavailable for `eth_call`.
    ///
    /// Currently, it affects both the storage logs (for Compacted state impl - see `state` crate for details)
    /// and repositories (see `repositories` package in this crate)
    #[config(default_t = 512)]
    pub blocks_to_retain_in_memory: usize,

    /// If set - initialize the configs based off the values from the yaml files from that directory.
    pub zkstack_cli_config_dir: Option<String>,

    /// **IMPORTANT: It must be set for an external node. However, setting this DOES NOT make the node into an external node.
    /// `SequencerConfig::block_replay_download_address` is the source of truth for node type. **
    #[config(default_t = None)]
    pub main_node_rpc_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum StateBackendConfig {
    FullDiffs,
    Compacted,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct GenesisConfig {
    /// L1 address of `Bridgehub` contract. This address and chain ID is an entrypoint into L1 discoverability so most
    /// other contracts should be discoverable through it.
    // TODO: Pre-configured value, to be removed. Optional(Serde![int]) is a temp hack, replace it with Serde![str] after removing the default.
    #[config(with = Optional(Serde![int]), default_t = Some("0x8bd76a67b984e8f0b902a82220a90fc45d9738a9".parse().unwrap()))]
    pub bridgehub_address: Option<Address>,

    /// Chain ID of the chain node operates on.
    #[config(default_t = Some(270))]
    pub chain_id: Option<u64>,

    /// Path to the file with genesis input.
    #[config(with = Optional(Serde![int]), default_t = Some("./genesis/genesis.json".into()))]
    pub genesis_input_path: Option<PathBuf>,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct StatusServerConfig {
    /// Status server address to listen on.
    #[config(default_t = "0.0.0.0:3071".into())]
    pub address: String,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
pub struct RebuildBlocksConfig {
    /// Number of the block to start rebuilding from.
    /// All blocks starting from this number will be replayed - but unlike normal replay,
    /// we'll not assert that the result will match the original ReplayRecord (block).
    /// That is, a block may close earlier (with less transactions),
    /// have different hash, have some transactions rejected etc
    pub from_block: u64,
    /// List of blocks to empty (i.e., remove all transactions from).
    #[config(default, with = Delimited(","))]
    pub blocks_to_empty: Vec<u64>,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct SequencerConfig {
    /// Where to download replays instead of actually running blocks.
    /// **Setting this makes the node into an external node.**
    #[config(default_t = None)]
    pub block_replay_download_address: Option<String>,

    /// Where to serve block replays (EN syncing protocol)
    #[config(default_t = "0.0.0.0:3053".into())]
    pub block_replay_server_address: String,

    /// Defines the block time for the sequencer.
    /// One of the block Seal Criteria. Only affects the Main Node.
    #[config(default_t = Duration::from_millis(100))]
    pub block_time: Duration,

    /// Max number of transactions in a block.
    /// One of the block Seal Criteria. Only affects the Main Node.
    #[config(default_t = 1000)]
    pub max_transactions_in_block: usize,

    /// Max gas used per block.
    /// One of the block Seal Criteria. Only affects the Main Node.
    #[config(default_t = 100_000_000)]
    pub block_gas_limit: u64,

    /// Max pubdata bytes per block.
    /// One of the block Seal Criteria. Only affects the Main Node.
    #[config(default_t = 110_000)]
    pub block_pubdata_limit_bytes: u64,

    /// Path to the directory where block dumps for unexpected failures will be saved.
    #[config(default_t = "./db/block_dumps".into())]
    pub block_dump_path: PathBuf,

    /// Address that receives the transaction fees.
    #[config(with = Serde![str], default_t = "0x36615Cf349d7F6344891B1e7CA7C72883F5dc049".parse().unwrap())]
    pub fee_collector_address: Address,

    /// Override for base fee (in wei). If set, base fee will be constant and equal to this value.
    pub base_fee_override: Option<u128>,

    /// Override for pubdata price (in wei). If set, pubdata price will be constant and equal to this value.
    pub pubdata_price_override: Option<u128>,

    /// Override for native price (in wei). If set, native price will be constant and equal to this value.
    pub native_price_override: Option<u128>,

    /// Maximum number of blocks to produce.
    /// `None` means unlimited (default, standard operations),
    /// `Some(0)` means no new blocks (useful when only RPC/replay/batching functionality is needed),
    /// `Some(n)` means seal at most n new blocks.
    /// Replay blocks are always processed regardless of this setting.
    /// Only affects the Main Node.
    /// Useful for mitigation/operations.
    #[config(default_t = None)]
    pub max_blocks_to_produce: Option<u64>,

    /// Enable REVM consistency checker.
    /// If enabled, an additional pipeline process will be executed after the sequencer.
    /// The process re-executes transactions on the REVM client and checks state diff consistency.
    /// If the state diffs are inconsistent, a warning or debug message will be logged, but it won't crash.
    /// The consistency checker propagates the output to the next pipeline item, so it is not a
    /// blocking process and the overhead should be small.
    #[config(default_t = false)]
    pub revm_consistency_checker_enabled: bool,

    /// Block rebuild options.
    #[config(nest)]
    pub block_rebuild: Option<RebuildBlocksConfig>,
}

impl SequencerConfig {
    pub fn is_main_node(&self) -> bool {
        self.block_replay_download_address.is_none()
    }
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct RpcConfig {
    /// JSON-RPC address to listen on.
    #[config(default_t = "0.0.0.0:3050".into())]
    pub address: String,

    /// Gas limit of transactions executed via eth_call
    #[config(default_t = 10000000)]
    pub eth_call_gas: usize,

    /// Number of concurrent API connections (passed to jsonrpsee, default value there is 128)
    #[config(default_t = 1000)]
    pub max_connections: u32,

    /// Maximum RPC request payload size for both HTTP and WS in megabytes
    #[config(default_t = 15)]
    pub max_request_size: u32,

    /// Maximum RPC response payload size for both HTTP and WS in megabytes
    #[config(default_t = 24)]
    pub max_response_size: u32,

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum RollupPubdataMode {
    Blobs,
    Calldata,
}

/// Only used on the Main Node.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct L1SenderConfig {
    /// Private key to commit batches to L1
    /// Must be consistent with the operator key set on the contract (permissioned!)
    // TODO: Pre-configured value, to be removed
    #[config(alias = "operator_private_key", default_t = "0xe3cfac06fad7427cc296c5ef0778f97c7bd59038294a746d59aafd8c8e64c96f".into())]
    pub operator_commit_pk: SecretString,

    /// Private key to use to submit proofs to L1
    /// Can be arbitrary funded address - proof submission is permissionless.
    // TODO: Pre-configured value, to be removed
    #[config(default_t = "0x18d5754d1941125257bea433b38e5c3562e5ddb096bc84d9d5cb497eab713f40".into())]
    pub operator_prove_pk: SecretString,

    /// Private key to use to execute batches on L1
    /// Can be arbitrary funded address - execute submission is permissionless.
    // TODO: Pre-configured value, to be removed
    #[config(default_t = "0x0d8523c8d72254293dd3fec214fe08c1eee7a813dfb959812f4a5c99024ab5e6".into())]
    pub operator_execute_pk: SecretString,

    /// Max fee per gas we are willing to spend (in gwei).
    #[config(default_t = 101)]
    pub max_fee_per_gas_gwei: u64,

    /// Max priority fee per gas we are willing to spend (in gwei).
    #[config(default_t = 2)]
    pub max_priority_fee_per_gas_gwei: u64,

    /// Max number of commands (to commit/prove/execute one batch) to be processed at a time.
    #[config(default_t = 16)]
    pub command_limit: usize,

    /// How often to poll L1 for new blocks.
    #[config(default_t = Duration::from_millis(100))]
    pub poll_interval: Duration,

    /// Whether L1 senders are enabled.
    /// Only affects the Main Node.
    /// Only useful for debug. When L1 senders are disabled,
    /// the node will eventually halt as produced batches are not processed further.
    #[config(default_t = true)]
    pub enabled: bool,

    /// Rollup pubdata mode - either blobs or calldata.
    #[config(default_t = RollupPubdataMode::Calldata)]
    #[config(with = Serde![str])]
    pub rollup_pubdata_mode: RollupPubdataMode,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct L1WatcherConfig {
    /// Max number of L1 blocks to be processed at a time.
    ///
    /// L1 providers have different limits:
    /// * Alchemy - 2k blocks per request
    /// * Chainstack - 10k blocks per request
    /// * reth (by default) - 100k blocks per request
    ///
    /// Overall, 1000 blocks is a fairly conservative default for the general case.
    #[config(default_t = 1000)]
    pub max_blocks_to_process: u64,

    /// How often to poll L1 for new priority requests.
    #[config(default_t = 100 * TimeUnit::Millis)]
    pub poll_interval: Duration,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct MempoolConfig {
    #[config(default_t = usize::MAX)]
    pub max_pending_txs: usize,
    #[config(default_t = usize::MAX)]
    pub max_pending_size: usize,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct TxValidatorConfig {
    /// Max input size of a transaction to be accepted by mempool
    #[config(default_t = 128 * 1024 * 1024)]
    pub max_input_bytes: usize,
}

/// Only used on the Main Node.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct BatcherConfig {
    /// How long to keep a batch open before sealing it.
    #[config(default_t = Duration::from_secs(1))]
    pub batch_timeout: Duration,

    /// Max number of blocks per batch
    #[config(default_t = 10)]
    pub blocks_per_batch_limit: u64,
}

/// Only used on the Main Node.
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

    /// Normally, the Prover input generator skips the blocks that are already FRI proved and committed to L1.
    /// When this option is enabled, it will reprocess all the blocks replayed by the node on startup.
    /// The number of blocks to replay on startup is configurable via `min_blocks_to_replay`.
    #[config(default_t = false)]
    pub force_process_old_blocks: bool,

    /// Path to the directory where RiscV binaries are unpacked (server_app.bin, app_data.bin, etc)
    #[config(default_t = "./db/app_bins".into())]
    pub app_bin_unpack_path: PathBuf,
}

/// Only used on the Main Node.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct ProverApiConfig {
    /// Prover API address to listen on.
    #[config(default_t = "0.0.0.0:3124".into())]
    pub address: String,

    /// Enabled by default.
    /// Use `prover_fake_fri_provers_enabled=false` to disable fake fri provers.
    #[config(nest)]
    pub fake_fri_provers: FakeFriProversConfig,

    #[config(nest)]
    /// Enabled by default.
    /// Use `prover_fake_snark_provers_enabled=false` to disable fake SNARK provers.
    ///
    /// Note that if SNARK provers are disabled but FRI fake provers are enabled,
    /// we'll still use fake SNARK proofs for fake FRI proofs -
    /// however, we won't turn real FRI proofs into fake ones - even on timeout.
    pub fake_snark_provers: FakeSnarkProversConfig,

    /// Timeout after which a prover job is assigned to another Fri Prover Worker.
    #[config(default_t = Duration::from_secs(300))]
    pub job_timeout: Duration,

    /// Max difference between the oldest and newest batch number being proven
    /// If the difference is larger than this, provers will not be assigned new jobs - only retries.
    /// We use max range instead of length limit to avoid having one old batch stuck -
    /// otherwise GaplessCommitter's buffer would grow indefinitely.
    #[config(default_t = 10)]
    pub max_assigned_batch_range: usize,

    /// Max number of FRI proofs that will be aggregated to a single SNARK job.
    #[config(default_t = 10)]
    pub max_fris_per_snark: usize,

    /// Default: backed by files under `./db/shared` folder.
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
    #[config(default_t = 5)]
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

    /// Only pick up jobs that are this time old.
    #[config(default_t = Duration::from_secs(10))]
    pub max_batch_age: Duration,
}

/// Set of options related to the observability stack,
/// e.g. logging, metrics, tracing, error tracking, etc.
#[derive(Debug, Clone, PartialEq, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct ObservabilityConfig {
    /// Configuration for Prometheus metrics.
    #[config(nest, default)]
    pub prometheus: PrometheusConfig,

    /// Configuration for Sentry error tracking.
    #[config(nest, default)]
    pub sentry: SentryConfig,

    /// Configuration for the logging stack.
    #[config(nest, default)]
    pub log: LogConfig,

    /// Configuration for the opentelemetry stack.
    #[config(nest, default)]
    pub otlp: OtlpConfig,
}

#[derive(Debug, Clone, PartialEq, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct PrometheusConfig {
    /// Port to expose Prometheus metrics on.
    #[config(default_t = 3312)]
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct SentryConfig {
    /// Sentry DSN URL.
    #[config(default_t = None)]
    pub dsn_url: Option<String>,

    /// Environment name for Sentry.
    #[config(default_t = None)]
    pub environment: Option<String>,
}

/// Configuration for the logging stack.
#[derive(Debug, Clone, PartialEq, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct LogConfig {
    /// Format of the logs emitted by the node.
    #[config(default)]
    #[config(with = Serde![str])]
    pub format: LogFormat,

    /// Whether to use color in logs.
    #[config(default_t = true)]
    pub use_color: bool,
}

/// Configuration for gas adjuster.
#[derive(Debug, Clone, PartialEq, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct GasAdjusterConfig {
    #[config(default_t = 100)]
    pub max_base_fee_samples: usize,
    #[config(default_t = 100)]
    pub num_samples_for_blob_base_fee_estimate: usize,
    #[config(default_t = 13 * TimeUnit::Seconds)]
    pub poll_period: Duration,
    #[config(default_t = 1.0)]
    pub pubdata_pricing_multiplier: f64,
}

/// Configuration for the opentelemetry stack.
#[derive(Debug, Clone, PartialEq, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct OtlpConfig {
    /// Level of spans to be exported to OpenTelemetry.
    /// Note that it works on top of the global log level filter.
    #[config(default)]
    #[config(with = Serde![str])]
    pub level: OpenTelemetryLevel,

    /// Endpoint to send traces to.
    #[config(default_t = None)]
    pub tracing_endpoint: Option<String>,

    /// Endpoint to send logs to.
    #[config(default_t = None)]
    pub logging_endpoint: Option<String>,
}

impl From<RpcConfig> for zksync_os_rpc::RpcConfig {
    fn from(c: RpcConfig) -> Self {
        Self {
            address: c.address,
            eth_call_gas: c.eth_call_gas,
            max_connections: c.max_connections,
            max_request_size: c.max_request_size,
            max_response_size: c.max_response_size,
            max_blocks_per_filter: c.max_blocks_per_filter,
            max_logs_per_response: c.max_logs_per_response,
            stale_filter_ttl: c.stale_filter_ttl,
        }
    }
}

impl From<SequencerConfig> for zksync_os_sequencer::config::SequencerConfig {
    fn from(c: SequencerConfig) -> Self {
        Self {
            block_time: c.block_time,
            max_transactions_in_block: c.max_transactions_in_block,
            block_dump_path: c.block_dump_path,
            block_replay_server_address: c.block_replay_server_address,
            block_replay_download_address: c.block_replay_download_address,
            block_gas_limit: c.block_gas_limit,
            block_pubdata_limit_bytes: c.block_pubdata_limit_bytes,
            max_blocks_to_produce: c.max_blocks_to_produce,
        }
    }
}

impl L1SenderConfig {
    fn into_lib_l1_sender_config<Input>(
        self,
        operator_pk: SecretString,
    ) -> zksync_os_l1_sender::config::L1SenderConfig<Input> {
        zksync_os_l1_sender::config::L1SenderConfig {
            operator_pk,
            max_fee_per_gas_gwei: self.max_fee_per_gas_gwei,
            max_priority_fee_per_gas_gwei: self.max_priority_fee_per_gas_gwei,
            command_limit: self.command_limit,
            poll_interval: self.poll_interval,
            phantom_data: Default::default(),
        }
    }
}
impl From<L1SenderConfig> for zksync_os_l1_sender::config::L1SenderConfig<CommitCommand> {
    fn from(c: L1SenderConfig) -> Self {
        let pk = c.operator_commit_pk.clone();
        c.into_lib_l1_sender_config(pk)
    }
}

impl From<L1SenderConfig> for zksync_os_l1_sender::config::L1SenderConfig<ProofCommand> {
    fn from(c: L1SenderConfig) -> Self {
        let pk = c.operator_prove_pk.clone();
        c.into_lib_l1_sender_config(pk)
    }
}
impl From<L1SenderConfig> for zksync_os_l1_sender::config::L1SenderConfig<ExecuteCommand> {
    fn from(c: L1SenderConfig) -> Self {
        let pk = c.operator_execute_pk.clone();
        c.into_lib_l1_sender_config(pk)
    }
}

impl From<L1WatcherConfig> for zksync_os_l1_watcher::L1WatcherConfig {
    fn from(c: L1WatcherConfig) -> Self {
        Self {
            max_blocks_to_process: c.max_blocks_to_process,
            poll_interval: c.poll_interval,
        }
    }
}

impl From<MempoolConfig> for zksync_os_mempool::PoolConfig {
    fn from(c: MempoolConfig) -> Self {
        Self {
            pending_limit: SubPoolLimit::new(c.max_pending_txs, c.max_pending_size),
            ..Default::default()
        }
    }
}

impl From<TxValidatorConfig> for zksync_os_mempool::TxValidatorConfig {
    fn from(c: TxValidatorConfig) -> Self {
        Self {
            max_input_bytes: c.max_input_bytes,
        }
    }
}

impl From<RebuildBlocksConfig> for RebuildOptions {
    fn from(c: RebuildBlocksConfig) -> Self {
        Self {
            rebuild_from_block: c.from_block,
            blocks_to_empty: c.blocks_to_empty.into_iter().collect(),
        }
    }
}

pub fn gas_adjuster_config(
    c: GasAdjusterConfig,
    da_input_mode: BatchDaInputMode,
    rollup_pubdata_mode: RollupPubdataMode,
    max_priority_fee_per_gas_gwei: u64,
) -> zksync_os_gas_adjuster::GasAdjusterConfig {
    let pubdata_mode = match (da_input_mode, rollup_pubdata_mode) {
        (BatchDaInputMode::Validium, _) => zksync_os_gas_adjuster::PubdataMode::Validium,
        (BatchDaInputMode::Rollup, RollupPubdataMode::Blobs) => {
            zksync_os_gas_adjuster::PubdataMode::Blobs
        }
        (BatchDaInputMode::Rollup, RollupPubdataMode::Calldata) => {
            zksync_os_gas_adjuster::PubdataMode::Calldata
        }
    };
    let max_priority_fee_per_gas = max_priority_fee_per_gas_gwei as u128 * (GWEI_TO_WEI as u128);
    zksync_os_gas_adjuster::GasAdjusterConfig {
        pubdata_mode,
        max_base_fee_samples: c.max_base_fee_samples,
        num_samples_for_blob_base_fee_estimate: c.num_samples_for_blob_base_fee_estimate,
        max_priority_fee_per_gas,
        poll_period: c.poll_period,
        pubdata_pricing_multiplier: c.pubdata_pricing_multiplier,
    }
}
