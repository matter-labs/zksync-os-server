pub use self::cli::ConfigArgs;
use self::util::{SecretKeyDeserializer, SignerConfigDeserializer};
use crate::{command_source::RebuildOptions, default_protocol_version::DEFAULT_ROCKS_DB_PATH};
use alloy::primitives::{Address, Bytes, U128};
use num::{BigInt, BigUint, rational::Ratio};
use reth_net_nat::net_if::resolve_net_if_ip;
use reth_network_peers::TrustedPeer;
use serde::{Deserialize, Serialize};
use smart_config::metadata::{SizeUnit, TimeUnit};
use smart_config::value::SecretString;
use smart_config::{
    ByteSize, ConfigRepository, ConfigSchema, ConfigSources, DescribeConfig, DeserializeConfig,
    EtherAmount, ParseErrors, Serde, de::Delimited, metadata::EtherUnit,
};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::{path::PathBuf, time::Duration};
use zksync_os_batch_verification;
use zksync_os_config_validation_macros::ConfigValidate;
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_l1_sender::commands::execute::ExecuteCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_mempool::SubPoolLimit;
use zksync_os_network::SecretKey;
use zksync_os_observability::LogFormat;
use zksync_os_observability::opentelemetry::OpenTelemetryLevel;
use zksync_os_operator_signer::SignerConfig;
use zksync_os_tx_validators::deployment_filter;
use zksync_os_types::{NodeRole, PubdataMode};

mod build_external_config;
mod cli;
mod util;

pub use build_external_config::{build_external_config, load_config_file_sources};

/// Configuration for the sequencer node.
/// Includes configurations of all subsystems.
///
/// ## Config loading order (later sources take precedence)
/// 1. **Defaults defined here** — production-oriented (testnet/mainnet).
/// 2. **Config files** passed via `--config`, applied in order. If `--config` is not provided and
///    `local-chains/{protocol_version}/default/config.yaml` exists on disk, that file (along with
///    `local-chains/local_dev.yaml`) is loaded automatically — this happens when running via
///    `cargo run` from the repo root. When `--config` is explicitly specified, only those
///    files are used. In Docker, the `local-chains/` directory is not copied into the image,
///    so no files are auto-loaded and config must be provided entirely via environment variables.
/// 3. **Environment variables** — always override everything.
#[derive(Debug, ConfigValidate)]
#[config_validate(root)]
pub struct Config {
    pub general_config: GeneralConfig,
    pub network_config: NetworkConfig,
    pub genesis_config: GenesisConfig,
    pub rpc_config: RpcConfig,
    pub mempool_config: MempoolConfig,
    pub tx_validator_config: MempoolTxValidatorConfig,
    pub sequencer_config: SequencerConfig,
    #[config_validate(async_validate(Self::validate_operator_signers))]
    pub l1_sender_config: L1SenderConfig,
    pub l1_watcher_config: L1WatcherConfig,
    pub batcher_config: BatcherConfig,
    pub prover_input_generator_config: ProverInputGeneratorConfig,
    pub prover_api_config: ProverApiConfig,
    pub status_server_config: StatusServerConfig,
    pub observability_config: ObservabilityConfig,
    pub gas_adjuster_config: GasAdjusterConfig,
    pub batch_verification_config: BatchVerificationConfig,
    pub base_token_price_updater_config: BaseTokenPriceUpdaterConfig,
    pub interop_fee_updater_config: InteropFeeUpdaterConfig,
    /// Only required on the Main Node, where the base token price updater runs.
    /// External Nodes never start that component and may omit this config entirely.
    #[config_validate(required_if = NodeRole::MainNode, skip_nested)]
    pub external_price_api_client_config: Option<ExternalPriceApiClientConfig>,
    pub fee_config: FeeConfig,
}

#[async_trait::async_trait(?Send)]
pub trait ConfigValidate {
    fn validate_conditional(&self, root: &Config, errors: &mut Vec<ValidationError>, prefix: &str);

    async fn validate_async(&self, _errors: &mut Vec<ValidationError>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn validate(&self) -> anyhow::Result<()>
    where
        Self: Sized + std::borrow::Borrow<Config>,
    {
        let root = self.borrow();
        let mut errors = Vec::new();
        Self::validate_conditional(self, root, &mut errors, "");
        self.validate_async(&mut errors).await?;
        if !errors.is_empty() {
            anyhow::bail!(format_validation_errors("invalid config", &errors));
        }
        Ok(())
    }
}

trait MaybeConditionalConfigValidator<T: ?Sized> {
    fn maybe_validate_conditional(
        &self,
        value: &T,
        root: &Config,
        errors: &mut Vec<ValidationError>,
        prefix: &str,
    );
}

impl<T: ConfigValidate + ?Sized> MaybeConditionalConfigValidator<T>
    for std::marker::PhantomData<T>
{
    fn maybe_validate_conditional(
        &self,
        value: &T,
        root: &Config,
        errors: &mut Vec<ValidationError>,
        prefix: &str,
    ) {
        value.validate_conditional(root, errors, prefix);
    }
}

impl<T: ?Sized> MaybeConditionalConfigValidator<T> for &std::marker::PhantomData<T> {
    fn maybe_validate_conditional(
        &self,
        _value: &T,
        _root: &Config,
        _errors: &mut Vec<ValidationError>,
        _prefix: &str,
    ) {
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    path: String,
    message: String,
}

impl ValidationError {
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` {}", self.path, self.message)
    }
}

pub(crate) fn join_validation_path(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_owned()
    } else {
        format!("{prefix}.{segment}")
    }
}

pub(crate) fn format_validation_errors(prefix: &str, errors: &[ValidationError]) -> String {
    let formatted_errors = errors
        .iter()
        .enumerate()
        .map(|(idx, error)| format!("{}. {error}", idx + 1))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{prefix}:\n{formatted_errors}")
}

impl Config {
    pub fn schema() -> ConfigSchema {
        let mut schema = ConfigSchema::default();
        schema
            .insert(&GeneralConfig::DESCRIPTION, "general")
            .expect("Failed to insert general config");
        schema
            .insert(&NetworkConfig::DESCRIPTION, "network")
            .expect("Failed to insert network config");
        schema
            .insert(&GenesisConfig::DESCRIPTION, "genesis")
            .expect("Failed to insert genesis config");
        schema
            .insert(&RpcConfig::DESCRIPTION, "rpc")
            .expect("Failed to insert rpc config");
        schema
            .insert(&MempoolConfig::DESCRIPTION, "mempool")
            .expect("Failed to insert mempool config");
        schema
            .insert(&MempoolTxValidatorConfig::DESCRIPTION, "tx_validator")
            .expect("Failed to insert tx_validator config");
        schema
            .insert(&SequencerConfig::DESCRIPTION, "sequencer")
            .expect("Failed to insert sequencer config");
        schema
            .insert(&L1SenderConfig::DESCRIPTION, "l1_sender")
            .expect("Failed to insert l1_sender config");
        schema
            .insert(&L1WatcherConfig::DESCRIPTION, "l1_watcher")
            .expect("Failed to insert l1_watcher config");
        schema
            .insert(&BatcherConfig::DESCRIPTION, "batcher")
            .expect("Failed to insert batcher config");
        schema
            .insert(
                &ProverInputGeneratorConfig::DESCRIPTION,
                "prover_input_generator",
            )
            .expect("Failed to insert prover_input_generator config");
        schema
            .insert(&ProverApiConfig::DESCRIPTION, "prover_api")
            .expect("Failed to insert prover api config");
        schema
            .insert(&StatusServerConfig::DESCRIPTION, "status_server")
            .expect("Failed to insert status server config");
        schema
            .insert(&ObservabilityConfig::DESCRIPTION, "observability")
            .expect("Failed to insert observability config");
        schema
            .insert(&GasAdjusterConfig::DESCRIPTION, "gas_adjuster")
            .expect("Failed to insert gas adjuster config");
        schema
            .insert(&BatchVerificationConfig::DESCRIPTION, "batch_verification")
            .expect("Failed to insert batch verification config");
        schema
            .insert(
                &BaseTokenPriceUpdaterConfig::DESCRIPTION,
                "base_token_price_updater",
            )
            .expect("Failed to insert base token price updater config");
        schema
            .insert(&InteropFeeUpdaterConfig::DESCRIPTION, "interop_fee_updater")
            .expect("Failed to insert interop fee updater config");
        schema
            .insert(
                &ExternalPriceApiClientConfig::DESCRIPTION,
                "external_price_api_client",
            )
            .expect("Failed to insert external price api client config");
        schema
            .insert(&FeeConfig::DESCRIPTION, "fee")
            .expect("Failed to insert fee config");
        schema
    }

    pub fn observability(sources: ConfigSources) -> anyhow::Result<ObservabilityConfig> {
        let schema = ConfigSchema::new(&ObservabilityConfig::DESCRIPTION, "observability");
        let repo = ConfigRepository::new(&schema).with_all(sources);
        repo.single()?.parse().map_err(log_all_errors)
    }

    async fn validate_operator_signers(
        root: &Self,
        l1_sender_config: &L1SenderConfig,
        errors: &mut Vec<ValidationError>,
    ) -> anyhow::Result<()> {
        if !root.general_config.node_role.is_main() {
            return Ok(());
        }

        let Some(commit) = &l1_sender_config.operator_commit_sk else {
            return Ok(());
        };
        let Some(prove) = &l1_sender_config.operator_prove_sk else {
            return Ok(());
        };
        let Some(execute) = &l1_sender_config.operator_execute_sk else {
            return Ok(());
        };

        let commit_addr = match commit.address().await {
            Ok(address) => Some(address),
            Err(err) => {
                errors.push(ValidationError::new(
                    "l1_sender.operator_commit_sk",
                    format!("failed to resolve signer address: {err}"),
                ));
                None
            }
        };
        let prove_addr = match prove.address().await {
            Ok(address) => Some(address),
            Err(err) => {
                errors.push(ValidationError::new(
                    "l1_sender.operator_prove_sk",
                    format!("failed to resolve signer address: {err}"),
                ));
                None
            }
        };
        let execute_addr = match execute.address().await {
            Ok(address) => Some(address),
            Err(err) => {
                errors.push(ValidationError::new(
                    "l1_sender.operator_execute_sk",
                    format!("failed to resolve signer address: {err}"),
                ));
                None
            }
        };

        if let (Some(commit_addr), Some(prove_addr), Some(execute_addr)) =
            (commit_addr, prove_addr, execute_addr)
            && (commit_addr == prove_addr
                || prove_addr == execute_addr
                || execute_addr == commit_addr)
        {
            errors.push(ValidationError::new(
                "l1_sender.operator_commit_sk",
                format!(
                    "must be different from `l1_sender.operator_prove_sk` and \
                     `l1_sender.operator_execute_sk`; got commit={commit_addr}, \
                     prove={prove_addr}, execute={execute_addr}"
                ),
            ));
        }

        Ok(())
    }
}

fn log_all_errors(errors: ParseErrors) -> anyhow::Error {
    const MAX_DISPLAYED_ERRORS: usize = 5;

    let mut displayed_errors = String::new();
    let mut error_count = 0;
    for (i, err) in errors.iter().enumerate() {
        tracing::error!(
            path = err.path(),
            origin = %err.origin(),
            config = err.config().ty.name_in_code(),
            param = err.param().map(|param| param.rust_field_name),
            "{}",
            err.inner()
        );

        if i < MAX_DISPLAYED_ERRORS {
            displayed_errors += &format!("{}. {err}\n", i + 1);
        }
        error_count += 1;
    }

    let maybe_truncation_message = if error_count > MAX_DISPLAYED_ERRORS {
        format!("; showing first {MAX_DISPLAYED_ERRORS} (all errors are logged at ERROR level)")
    } else {
        String::new()
    };

    anyhow::anyhow!(
        "failed parsing config param(s): {error_count} error(s) in total{maybe_truncation_message}\n{displayed_errors}"
    )
}

/// "Umbrella" config for the node.
/// If variable is shared i.e. used by multiple components OR does not belong to any specific component
/// then it belongs here.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig, ConfigValidate)]
#[config(derive(Default))]
pub struct GeneralConfig {
    #[config(default_t = NodeRole::MainNode, with = Serde![str])]
    pub node_role: NodeRole,

    /// L1's JSON RPC API.
    #[config(default_t = "http://localhost:8545".into())]
    pub l1_rpc_url: String,

    /// Poll interval used by the L1 alloy provider when waiting for transaction receipts.
    /// Alloy's default is 7 seconds for HTTP transports, using the same criteria here.
    #[config(default_t = 7 * TimeUnit::Seconds)]
    pub l1_rpc_poll_interval: Duration,

    /// Gateway's JSON RPC API.
    /// Must be present if the chain is currently settling to Gateway.
    pub gateway_rpc_url: Option<String>,

    /// Poll interval used by the Gateway alloy provider when waiting for transaction receipts.
    /// Alloy's default is 7 seconds for HTTP transports, using the same criteria here.
    #[config(default_t = 7 * TimeUnit::Seconds)]
    pub gateway_rpc_poll_interval: Duration,

    /// Gateway chain ID. Used by the migration watcher to construct `SetSLChainId` system
    /// transactions when a `MigrateToGateway` event fires. Defaults to 506 (ZKsync Gateway).
    #[config(default_t = 506)]
    pub gateway_chain_id: u64,

    /// Min number of blocks to replay on restart
    /// Depending on L1/persistence state, we may need to replay more blocks than this number
    /// In some cases, we need to replay the whole blockchain (e.g. switching state backends) -
    /// in such cases a warning is logged.
    #[config(default_t = 10)]
    pub min_blocks_to_replay: usize,

    /// Force a block number to start replaying from.
    /// Only FullDiffs backend is supported:
    ///     On EN: can be any historical block number;
    ///     On Main Node: any historical block number up to the last l1 executed one.
    #[config(default_t = None)]
    pub force_starting_block_number: Option<u64>,

    /// Path to the directory for persistence (eg RocksDB) - will contain both state and repositories' DBs
    #[config(default_t = DEFAULT_ROCKS_DB_PATH.into())]
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

    /// **IMPORTANT: It must be set for an external node. However, setting this DOES NOT make the node into an external node.
    /// [`GeneralConfig::node_role`] is the source of truth for node type. **
    #[config(default_t = None)]
    #[config_validate(required_if = NodeRole::ExternalNode)]
    pub main_node_rpc_url: Option<String>,

    /// Whether to run the priority tree component.
    /// Required for Main Node (will panic if false on Main Node).
    /// Optional for External Nodes - if disabled on EN, the priority tree will need to be rebuilt
    /// from scratch before turning this EN into a Main Node.
    #[config(default_t = true)]
    #[config_validate(custom(
        |root: &Config, value: &bool| !root.general_config.node_role.is_main() || *value,
        "must be true when `general.node_role=main`"
    ))]
    pub run_priority_tree: bool,

    /// Enables ephemeral mode that isolates RocksDB into a temporary directory.
    /// The directory is removed once the process shuts down.
    /// Disables all HTTP APIs except JSON RPC.
    #[config(default_t = false, alias = "sandbox")]
    pub ephemeral: bool,

    /// Path to ephemeral state to load at startup.
    #[config(default_t = None)]
    #[config_validate(custom(
        |root: &Config, value: &Option<PathBuf>| root.general_config.ephemeral || value.is_none(),
        "requires `general.ephemeral=true`"
    ))]
    pub ephemeral_state: Option<PathBuf>,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig, ConfigValidate)]
#[config(derive(Default))]
pub struct NetworkConfig {
    /// Whether devp2p-based networking should be enabled.
    #[config(default_t = false)]
    #[config_validate(custom(
        |root: &Config, value: &bool| !root.general_config.node_role.is_external() || *value,
        "must be true when `general.node_role=external`"
    ))]
    pub enabled: bool,
    /// The node's secret key (256-bit ECDSA), from which the node's identity is derived. Used during
    /// initial RLPx handshake.
    #[config(secret)]
    #[config(default, with = SecretKeyDeserializer)]
    #[config_validate(custom(
        |root: &Config, value: &Option<SecretKey>| !root.network_config.enabled || value.is_some(),
        "is required when `network.enabled=true`"
    ))]
    pub secret_key: Option<SecretKey>,
    /// IPv4 address to use for Node Discovery Protocol v5 (discv5) and RLPx Transport Protocol
    /// (rlpx).
    #[config(default_t = Ipv4Addr::UNSPECIFIED, with = Serde![str])]
    pub address: Ipv4Addr,
    /// Optional networking interface override. If set, this is used instead of `network.address`.
    /// The interface IP is resolved at startup; for example, `eth0` may resolve to a private LAN
    /// address such as `172.16.1.12`.
    #[config(default_t = None)]
    pub interface: Option<String>,
    /// Port to use for Node Discovery Protocol v5 (discv5) and RLPx Transport Protocol (rlpx).
    #[config(default_t = 3060)]
    pub port: u16,
    /// All boot nodes to start network discovery with. Expected format is
    /// `enode://<node ID>@<IP address>:<port>` or `enode://<node ID>@<DNS name>:<port>`
    /// delimited by commas (`,`). DNS names are resolved by the networking stack. For example:
    /// `enode://dbd18888f17bad7df7fa958b57f4993f47312ba5364508fd0d9027e62ea17a037ca6985d6b0969c4341f1d4f8763a802785961989d07b1fb5373ced9d43969f6@127.0.0.1:3060`
    #[config(
        default,
        with = Delimited::repeat(Serde![str], ",")
    )]
    pub boot_nodes: Vec<TrustedPeer>,
}

impl NetworkConfig {
    fn resolved_address(&self) -> Ipv4Addr {
        let Some(interface) = self.interface.as_deref() else {
            return self.address;
        };

        match resolve_net_if_ip(interface).unwrap_or_else(|err| {
            panic!("failed to resolve network interface '{interface}': {err}")
        }) {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(ip) => panic!(
                "failed to resolve network interface '{interface}': resolved to unsupported IPv6 address {ip}"
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum StateBackendConfig {
    FullDiffs,
    Compacted,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig, ConfigValidate)]
pub struct GenesisConfig {
    /// L1 address of `Bridgehub` contract. This address and chain ID is an entrypoint into L1 discoverability so most
    /// other contracts should be discoverable through it.
    #[config_validate(required_if = NodeRole::MainNode)]
    pub bridgehub_address: Option<Address>,

    /// L1 address of the `BytecodeSupplier` contract. This address right now cannot be discovered through `Bridgehub`,
    /// so it has to be provided explicitly.
    #[config_validate(required_if = NodeRole::MainNode)]
    pub bytecode_supplier_address: Option<Address>,

    /// Chain ID of the chain node operates on.
    #[config_validate(required_if = NodeRole::MainNode)]
    pub chain_id: Option<u64>,

    /// Path to the file with genesis input.
    #[config_validate(required_if = NodeRole::MainNode)]
    pub genesis_input_path: Option<PathBuf>,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct StatusServerConfig {
    /// Whether to enable status server.
    #[config(default_t = true)]
    pub enabled: bool,

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
    #[config(default, with = Delimited::new(","))]
    pub blocks_to_empty: Vec<u64>,
    /// Whether to reset timestamps of rebuilt blocks to the current time.
    /// If false, original timestamps are kept. If true then current time is used.
    #[config(default_t = false)]
    pub reset_timestamps: bool,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig, ConfigValidate)]
#[config(derive(Default))]
pub struct SequencerConfig {
    /// Defines the block time for the sequencer.
    /// One of the block Seal Criteria. Only affects the Main Node.
    #[config(default_t = Duration::from_millis(250))]
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

    /// Maximum number of blocks to produce.
    /// `None` means unlimited (default, standard operations),
    /// `Some(0)` means no new blocks (useful when only RPC/replay/batching functionality is needed),
    /// `Some(n)` means seal at most n new blocks.
    /// Replay blocks are always processed regardless of this setting.
    /// Only affects the Main Node.
    /// Useful for mitigation/operations.
    #[config(default_t = None)]
    pub max_blocks_to_produce: Option<u64>,

    /// Max number of interop roots to be included in a single transaction
    #[config(default_t = 100)]
    pub interop_roots_per_tx: usize,

    /// Delay between 2 consecutive service blocks.
    /// Defaults to 3 times of usual block time, to allow passing other transactions in between
    #[config(default_t = Duration::from_millis(750))]
    pub service_block_delay: Duration,

    /// Enable REVM consistency checker.
    /// If enabled, an additional pipeline process will be executed after the sequencer.
    /// The process re-executes transactions on the REVM client and checks state diff consistency.
    /// If the state diffs are inconsistent, a warning or debug message will be logged, but it won't crash.
    /// The consistency checker propagates the output to the next pipeline item, so it is not a
    /// blocking process and the overhead should be small.
    #[config(default_t = true)]
    pub revm_consistency_checker_enabled: bool,
    /// If enabled, node will revert block with divergence detected by REVM consistency checker.
    #[config(default_t = false)]
    pub revm_consistency_checker_revert_on_divergence: bool,

    /// Block rebuild options.
    #[config(nest)]
    pub block_rebuild: Option<RebuildBlocksConfig>,

    /// If set, external node will sync up to and including this block number and then stop processing blocks.
    #[config(default)]
    pub en_sync_up_to_block: Option<u64>,

    #[config(default, with = Serde![*])]
    /// List of (block_number, db_key) pairs to override when downloading replay records.
    pub en_replay_record_overrides: Vec<(u64, Bytes)>,

    /// Transaction validator configuration.
    #[config(nest)]
    pub tx_validator: TxValidatorConfig,
}

/// Configuration for all transaction validators applied during block production.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig, ConfigValidate)]
#[config(derive(Default))]
pub struct TxValidatorConfig {
    /// Deployment filter configuration.
    #[config(nest)]
    pub deployment_filter: DeploymentFilterConfig,
}

/// Configuration for the deployment filter.
/// When enabled, only transactions from allowed deployers can deploy contracts.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct DeploymentFilterConfig {
    /// Whether the deployment filter is enabled.
    #[config(default_t = false)]
    pub enabled: bool,

    /// List of addresses allowed to deploy contracts.
    #[config(default, with = Serde![*])]
    pub allowed_deployers: Vec<Address>,
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
    #[config(default_t = 10_000)]
    pub max_blocks_per_filter: u64,

    /// Maximum number of logs that can be returned in a response
    #[config(default_t = 10_000)]
    pub max_logs_per_response: usize,

    /// Duration since the last filter poll, after which the filter is considered stale
    #[config(default_t = 15 * TimeUnit::Minutes)]
    pub stale_filter_ttl: Duration,

    /// List of L2 signer addresses to blacklist (i.e. their transactions are rejected).
    #[config(default, with = Delimited::new(","))]
    pub l2_signer_blacklist: HashSet<Address>,

    /// Default timeout for `eth_sendRawTransactionSync`
    #[config(default_t = 2 * TimeUnit::Seconds)]
    pub send_raw_transaction_sync_timeout: Duration,

    /// Factor applied to the pending block base fee returned by `eth_gasPrice`.
    /// Some tools, e.g. Metamask, submit transactions with `maxFeePerGas=eth_gasPrice`, so it's important for multiplier to be `> 1`.
    #[config(default_t = 1.5)]
    pub gas_price_scale_factor: f64,

    /// Factor for pubdata price used during gas limit estimation (`eth_estimateGas`).
    /// Needed to account for pubdata price market fluctuations.
    /// Pubdata price can increase for up to 50% between consecutive blocks, native price can decrease for up to 12.5% ->
    /// `native_per_pubdata` can increase in 1.5/0.875=1.714 times.
    /// Setting it to a smaller value will increase the probability of users submitting
    /// unexecutable/failing transactions (usually fail with `OutOfNativeResourcesDuringValidation`)
    /// because pubdata price increases or native price decreases in-between estimation and sequencing.
    #[config(default_t = 2.0)]
    pub estimate_gas_pubdata_price_factor: f64,
}

/// L1 sender configuration. The signing key fields are only required on the Main Node;
/// External Nodes do not send L1 transactions and may omit them.
///
/// Each operator accepts either a hex private key string (backward-compatible) or a GCP KMS
/// resource object: `{"type": "gcp_kms", "resource": "projects/.../cryptoKeyVersions/N"}`.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig, ConfigValidate)]
pub struct L1SenderConfig {
    /// Signer to commit batches to L1.
    /// Must be consistent with the operator key set on the contract (permissioned!)
    /// Not required for External Nodes, which do not send L1 transactions.
    #[config_validate(required_if = NodeRole::MainNode)]
    #[config(secret, alias = "operator_commit_pk", with = SignerConfigDeserializer)]
    pub operator_commit_sk: Option<SignerConfig>,

    /// Signer to submit proofs to L1.
    /// Can be arbitrary funded address - proof submission is permissionless.
    /// Not required for External Nodes, which do not send L1 transactions.
    #[config_validate(required_if = NodeRole::MainNode)]
    #[config(secret, alias = "operator_prove_pk", with = SignerConfigDeserializer)]
    pub operator_prove_sk: Option<SignerConfig>,

    /// Signer to execute batches on L1.
    /// Can be arbitrary funded address - execute submission is permissionless.
    /// Not required for External Nodes, which do not send L1 transactions.
    #[config_validate(required_if = NodeRole::MainNode)]
    #[config(secret, alias = "operator_execute_pk", with = SignerConfigDeserializer)]
    pub operator_execute_sk: Option<SignerConfig>,

    /// Max fee per gas we are willing to spend.
    #[config(default_t = 200 * EtherUnit::Gwei)]
    pub max_fee_per_gas: EtherAmount,

    /// Max priority fee per gas we are willing to spend.
    #[config(default_t = 1 * EtherUnit::Gwei)]
    pub max_priority_fee_per_gas: EtherAmount,

    /// Max fee per blob gas we are willing to spend.
    #[config(default_t = 2 * EtherUnit::Gwei)]
    pub max_fee_per_blob_gas: EtherAmount,

    /// Max number of commands (to commit/prove/execute one batch) to be processed at a time.
    #[config(default_t = 16)]
    pub command_limit: usize,

    /// How often to poll L1 for new blocks.
    #[config(default_t = 1 * TimeUnit::Seconds)]
    pub poll_interval: Duration,

    /// Maximum time to wait for an L1 transaction to be included.
    ///
    /// Normally 15-30 seconds is sufficient for normal-priority transactions; 60-120s covers most
    /// lower-gas-price cases. 600 seconds is a conservative default that handles heavy congestion.
    #[config(default_t = 600 * TimeUnit::Seconds)]
    pub transaction_timeout: Duration,

    /// Use Fusaka blob transaction format if the timestamp has passed.
    ///
    /// Defaults to `2^64-1` which is practically never. This is needed for local setup as anvil
    /// does not support EIP-7594 yet (https://github.com/foundry-rs/foundry/issues/12222).
    #[config(default_t = u64::MAX)]
    pub fusaka_upgrade_timestamp: u64,

    /// Whether L1 senders are enabled.
    /// Only affects the Main Node.
    /// Only useful for debug. When L1 senders are disabled,
    /// the node will eventually halt as produced batches are not processed further.
    #[config(default_t = true)]
    pub enabled: bool,

    /// Pubdata mode is used by block-producing components on the Main Node.
    /// External Nodes only replay blocks, so they may leave this unset.
    #[config_validate(required_if = NodeRole::MainNode)]
    #[config(with = Serde![str])]
    pub pubdata_mode: Option<PubdataMode>,
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

    /// Number of latest L1 blocks to leave unprocessed in order to reduce reorg risk.
    #[config(default_t = 2)]
    pub confirmations: u64,

    /// How often to poll L1 for new priority requests.
    #[config(default_t = 1 * TimeUnit::Seconds)]
    pub poll_interval: Duration,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct MempoolConfig {
    #[config(default_t = usize::MAX)]
    pub max_pending_txs: usize,
    #[config(default_t = usize::MAX)]
    pub max_pending_size: usize,
    /// Minimal fee per gas (in WEI) for a transaction to be accepted by mempool
    /// Defaults to `7` which is the lowest possible value of base fee under mainnet EIP-1559 params
    #[config(default_t = 7)]
    pub minimal_protocol_basefee: u64,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct MempoolTxValidatorConfig {
    /// Max input size of a transaction to be accepted by mempool
    #[config(default_t = 128 * 1024 * 1024)]
    pub max_input_bytes: usize,
}

/// Only used on the Main Node.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct BatcherConfig {
    /// Whether to run the batcher subsystem and all downstream components (prover input
    /// generation, L1 settlement, priority tree, etc.). Defaults to `true`.
    /// Set to `false` to run the node without committing batches to L1 — useful for
    /// testing or operating a read-only / replay-only node.
    #[config(default_t = true)]
    pub enabled: bool,

    /// Maximum time a batch stays open before being sealed.
    ///
    /// The deadline is computed as `first_block_timestamp + batch_timeout`, where
    /// `first_block_timestamp` is the L2 timestamp of the first block in the batch. Using an
    /// L2 timestamp as the anchor makes the deadline restart-resilient: it does not reset when
    /// the server restarts mid-batch.
    ///
    /// Set this to `settlement_sli - expected_proving_time - expected_l1_submission_time`.
    /// The operator is responsible for reserving enough headroom for proving and L1 submission,
    /// since proving time is non-deterministic and cannot be reliably modelled as a fixed buffer.
    ///
    /// On mainnet environments with low load, consider a higher value (e.g. 3 hours), as L1
    /// settlement has a non-trivial gas overhead per batch.
    #[config(default_t = 60 * TimeUnit::Seconds)]
    pub batch_timeout: Duration,

    /// Max number of transactions per batch
    #[config(default_t = 10000)]
    pub tx_per_batch_limit: u64,

    /// Max number of interop roots per batch
    #[config(default_t = 1000)]
    pub interop_roots_per_batch_limit: u64,

    /// Whether to verify that rebuilt batches match stored batches by comparing hashes.
    /// Enabled by default for safety. Disabling this check can be useful for debugging or
    /// when recovering from corrupted state.
    #[config(default_t = true)]
    pub assert_rebuilt_batch_hashes: bool,
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

    /// When false, skip prover input generation and emit `ProverInput::Fake` instead.
    /// Used for tests and some testnets where the expensive RiscV witness computation
    /// is unnecessary.
    #[config(default_t = true)]
    pub enable_input_generation: bool,
}

/// Only used on the Main Node.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig, ConfigValidate)]
#[config(derive(Default))]
pub struct ProverApiConfig {
    /// Whether to enable prover server.
    #[config(default_t = true)]
    pub enabled: bool,

    /// Prover API address to listen on.
    #[config(default_t = "0.0.0.0:3124".into())]
    pub address: String,

    /// Pool of in-process fake FRI provers that instantly produce dummy proofs, bypassing real proving.
    /// Useful for local development and testnets where real provers are unavailable or insufficient.
    #[config(nest)]
    pub fake_fri_provers: FakeFriProversConfig,

    #[config(nest)]
    /// Pool of in-process fake SNARK provers that instantly produce dummy proofs, bypassing real proving.
    /// Useful for local development and testnets where real provers are unavailable or insufficient.
    /// Must not be enabled on mainnets (Fake proofs will be rejected by L1)
    ///
    /// Note that if SNARK provers are disabled but FRI fake provers are enabled,
    /// we'll still use fake SNARK proofs for fake FRI proofs -
    /// however, we won't turn real FRI proofs into fake ones - even on timeout.
    pub fake_snark_provers: FakeSnarkProversConfig,

    /// Timeout after which a FRI prover job is assigned to another Fri Prover Worker.
    #[config(alias = "job_timeout", default_t = 60 * TimeUnit::Seconds)]
    pub fri_job_timeout: Duration,

    /// Timeout after which a SNARK prover job is assigned to another SNARK Prover Worker.
    #[config(default_t = Duration::from_secs(300))]
    pub snark_job_timeout: Duration,

    /// Max difference between the oldest and newest batch number being proven
    /// If the difference is larger than this, provers will not be assigned new jobs - only retries.
    /// We use max range instead of length limit to avoid having one old batch stuck -
    /// otherwise GaplessCommitter's buffer would grow indefinitely.
    #[config(default_t = 10)]
    pub max_assigned_batch_range: usize,

    /// Max number of FRI proofs that will be aggregated to a single SNARK job.
    #[config(default_t = 10)]
    pub max_fris_per_snark: usize,

    /// Default: store files in ./db/fri_proofs/ with 1GiB disk usage cap
    #[config(nest, default)]
    pub proof_storage: ProofStorageConfig,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct FakeFriProversConfig {
    /// Whether to enable the fake provers pool.
    #[config(default_t = false)]
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

    /// Probability (0.0 to 1.0) that a job will timeout/be dropped instead of submitting a proof.
    /// 0.0 means never timeout (default behavior).
    /// For example, 0.1 means 10% of jobs will be dropped.
    /// Used to test queuing behavior on timeout.
    #[config(default_t = 0.0)]
    pub timeout_frequency: f64,
}

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct FakeSnarkProversConfig {
    /// Whether to enable the fake provers pool.
    #[config(default_t = false)]
    pub enabled: bool,

    /// Only pick up jobs that are this time old.
    #[config(default_t = Duration::from_secs(10))]
    pub max_batch_age: Duration,
}

#[derive(Debug, Clone, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct ProofStorageConfig {
    #[config(default_t = "./db/fri_proofs/".into())]
    pub path: PathBuf,
    /// The disk usage in bytes for batches with proofs,
    /// old entries are removed to keep usage capped
    #[config(default_t = 1 * SizeUnit::GiB)]
    pub batch_with_proof_capacity: ByteSize,
    /// The disk usage in bytes for failed proofs,
    /// old entries are removed to keep usage capped
    #[config(default_t = 1 * SizeUnit::GiB)]
    pub failed_capacity: ByteSize,
}

/// Set of options related to the observability stack,
/// e.g. logging, metrics, tracing, error tracking, etc.
#[derive(Debug, Clone, PartialEq, DescribeConfig, DeserializeConfig, ConfigValidate)]
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
    #[config(default_t = LogFormat::Json)]
    #[config(with = Serde![str])]
    pub format: LogFormat,

    /// Whether to use color in logs.
    #[config(default_t = false)]
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
    #[config(default_t = 100)]
    pub max_blob_fill_ratio_samples: usize,
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

/// Configuration for batch verification over the p2p network.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig, ConfigValidate)]
#[config(derive(Default))]
pub struct BatchVerificationConfig {
    /// [main node] If we are collecting batch verification signatures.
    #[config(default_t = false)]
    #[config_validate(custom(
        |root: &Config, value: &bool| !*value || root.network_config.enabled,
        "requires `network.enabled=true`"
    ))]
    pub server_enabled: bool,
    /// [external node] If we are signing batches.
    #[config(default_t = false)]
    #[config_validate(custom(
        |root: &Config, value: &bool| !*value || root.network_config.enabled,
        "requires `network.enabled=true`"
    ))]
    pub client_enabled: bool,
    /// [main node] Threshold (number of needed signatures).
    #[config(default_t = 1)]
    pub threshold: u64,
    /// [main node] Accepted signer pubkeys.
    #[config(default_t = vec!["0x36615Cf349d7F6344891B1e7CA7C72883F5dc049".into()], with = Delimited::new(","))]
    pub accepted_signers: Vec<String>,
    /// [main node] Iteration timeout.
    #[config(default_t = Duration::from_secs(5))]
    pub request_timeout: Duration,
    /// [main node] Retry delay between attempts.
    #[config(default_t = Duration::from_secs(1))]
    pub retry_delay: Duration,
    /// [main node] Total timeout.
    #[config(default_t = Duration::from_secs(300))]
    pub total_timeout: Duration,
    /// [external node] Signing key.
    // default address 0x36615Cf349d7F6344891B1e7CA7C72883F5dc049
    #[config(default_t = "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110".into())]
    pub signing_key: SecretString,
}

/// Config for the base token price updater.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct BaseTokenPriceUpdaterConfig {
    /// How often to fetch external prices.
    #[config(default_t = Duration::from_secs(30))]
    pub price_polling_interval: Duration,
    /// How many percent a quote needs to change in order for update to be propagated to L1.
    /// Exists to save on gas.
    #[config(default_t = 10)]
    pub l1_update_deviation_percentage: u32,
    /// Maximum number of attempts to fetch quote from a remote API before failing over.
    #[config(default_t = 3)]
    pub price_fetching_max_attempts: u32,
    /// Override for address of the base token address.
    pub base_token_addr_override: Option<Address>,
    /// Override for decimals of the base token.
    pub base_token_decimals_override: Option<u8>,
    /// Override for address of the gateway base token address used to calculate ETH<->GatewayBaseToken ratio on gateway using chains.
    pub gateway_base_token_addr_override: Option<Address>,
    /// Signer to update base token price on L1.
    /// Must be consistent with the key set on the chain admin contract.
    /// Not used for chains with ETH as base token; expected to be set for all other chains.
    /// Accepts either a hex private key string or a GCP KMS resource object.
    #[config(secret, alias = "token_multiplier_setter_pk", with = SignerConfigDeserializer)]
    pub token_multiplier_setter_sk: Option<SignerConfig>,
    /// Predefined fallback prices for tokens in case external API fetching fails on startup.
    #[config(default, with = Serde![*])]
    pub fallback_prices: HashMap<Address, f64>,
}

/// Config for the interop fee updater.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct InteropFeeUpdaterConfig {
    /// How often to check whether interop fee should be updated.
    #[config(default_t = Duration::from_secs(30))]
    pub polling_interval: Duration,
    /// Minimum percent deviation required to enqueue a new interop fee transaction.
    #[config(default_t = 10)]
    pub update_deviation_percentage: u32,
}

/// Config to force configured token prices in USD.
/// E.g. if needed to force 1 TOKEN = 0.3 USD, that would be represented in a config with price=0.3 for this token.
/// Important: price is **token** price (e.g. for USDC it would be 1), not base token unit price.
#[derive(Debug, Clone, PartialEq, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct ForcedPriceClientConfig {
    /// Map of token addresses to their forced price in USD for 1 token (not base token unit!).
    #[config(default, with = Serde![*])]
    pub prices: HashMap<Address, f64>,
    /// Forced fluctuation. It defines how much percent the ratio should fluctuate from its forced
    /// value. If it's 0, then the ForcedPriceClient will return the same quote every time
    /// it's called. Otherwise, ForcedPriceClient will return quote with numerator +/- fluctuation %.
    #[config(default_t = 20.0)]
    pub fluctuation: f64,
    /// In order to smooth out fluctuation, consecutive values returned by forced client will not
    /// differ more than next_value_fluctuation percent.
    #[config(default_t = 5.0)]
    pub next_value_fluctuation: f64,
}

/// Configuration for external price API client.
#[derive(Debug, Clone, DescribeConfig, DeserializeConfig)]
#[config(tag = "source")]
pub enum ExternalPriceApiClientConfig {
    Forced {
        /// Config for forced price client.
        #[config(nest)]
        forced: ForcedPriceClientConfig,
    },
    CoinGecko {
        /// Base URL of the external price API.
        base_url: Option<String>,
        /// API key for the external price API.
        coingecko_api_key: Option<SecretString>,
        /// Timeout for the external price API client.
        #[config(default_t = Duration::from_secs(10))]
        client_timeout: Duration,
    },
    CoinMarketCap {
        /// Base URL of the external price API.
        base_url: Option<String>,
        /// API key for the external price API. Required.
        cmc_api_key: SecretString,
        /// Timeout for the external price API client.
        #[config(default_t = Duration::from_secs(10))]
        client_timeout: Duration,
    },
}

/// Fee-related configuration.
#[derive(Debug, Clone, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct FeeConfig {
    /// Price for one unit of native resource in USD.
    /// Default is set based on the current estimate of proving price.
    #[config(default_t = 3e-9)]
    pub native_price_usd: f64,
    /// Override for base fee (in base token units).
    /// If set, base fee will be constant and equal to this value.
    pub base_fee_override: Option<U128>,
    /// Defines how many native resource units are equivalent to one gas unit in terms of price.
    #[config(default_t = 100)]
    pub native_per_gas: u64,
    /// Override for pubdata price (in base token units).
    /// If set, pubdata price will be constant and equal to this value.
    pub pubdata_price_override: Option<U128>,
    /// Cap for pubdata price (in base token units). If set, pubdata price will not exceed this value.
    /// Note:
    /// - has no effect if `pubdata_price_override` is set.
    /// - if pubdata cap is reached, chain operator may operate at a loss.
    pub pubdata_price_cap: Option<U128>,
    /// Override for native price (in base token units).
    /// If set, native price will be constant and equal to this value.
    pub native_price_override: Option<U128>,
}

impl From<NetworkConfig> for zksync_os_network::config::NetworkConfig {
    fn from(value: NetworkConfig) -> Self {
        Self {
            secret_key: value
                .secret_key
                .expect("`network.secret_key` is required for running p2p networking stack"),
            address: value.resolved_address(),
            port: value.port,
            boot_nodes: value.boot_nodes,
        }
    }
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
            l2_signer_blacklist: c.l2_signer_blacklist,
            stale_filter_ttl: c.stale_filter_ttl,
            send_raw_transaction_sync_timeout: c.send_raw_transaction_sync_timeout,
            gas_price_scale_factor: c.gas_price_scale_factor,
            estimate_gas_pubdata_price_factor: c.estimate_gas_pubdata_price_factor,
        }
    }
}

impl From<&Config> for zksync_os_sequencer::config::SequencerConfig {
    fn from(c: &Config) -> Self {
        Self {
            node_role: c.general_config.node_role,
            block_time: c.sequencer_config.block_time,
            max_transactions_in_block: c.sequencer_config.max_transactions_in_block,
            block_dump_path: c.sequencer_config.block_dump_path.clone(),
            block_gas_limit: c.sequencer_config.block_gas_limit,
            block_pubdata_limit_bytes: c.sequencer_config.block_pubdata_limit_bytes,
            max_blocks_to_produce: c.sequencer_config.max_blocks_to_produce,
            interop_roots_per_tx: c.sequencer_config.interop_roots_per_tx,
            tx_validator: {
                let df = &c.sequencer_config.tx_validator.deployment_filter;
                zksync_os_sequencer::config::TxValidatorConfig {
                    deployment_filter: if df.enabled {
                        deployment_filter::Config::allow_list(df.allowed_deployers.iter().copied())
                    } else {
                        deployment_filter::Config::Unrestricted
                    },
                }
            },
        }
    }
}

impl L1SenderConfig {
    fn into_lib_l1_sender_config<Input>(
        self,
        operator_signer: SignerConfig,
    ) -> zksync_os_l1_sender::config::L1SenderConfig<Input> {
        zksync_os_l1_sender::config::L1SenderConfig {
            operator_signer,
            max_fee_per_gas_wei: self.max_fee_per_gas.0,
            max_priority_fee_per_gas_wei: self.max_priority_fee_per_gas.0,
            max_fee_per_blob_gas_wei: self.max_fee_per_blob_gas.0,
            command_limit: self.command_limit,
            poll_interval: self.poll_interval,
            transaction_timeout: self.transaction_timeout,
            fusaka_upgrade_timestamp: self.fusaka_upgrade_timestamp,
            phantom_data: Default::default(),
        }
    }
}

impl From<L1SenderConfig> for zksync_os_l1_sender::config::L1SenderConfig<CommitCommand> {
    fn from(c: L1SenderConfig) -> Self {
        let signer = c
            .operator_commit_sk
            .clone()
            .expect("operator_commit_sk must be set on the Main Node");
        c.into_lib_l1_sender_config(signer)
    }
}

impl From<L1SenderConfig> for zksync_os_l1_sender::config::L1SenderConfig<ProofCommand> {
    fn from(c: L1SenderConfig) -> Self {
        let signer = c
            .operator_prove_sk
            .clone()
            .expect("operator_prove_sk must be set on the Main Node");
        c.into_lib_l1_sender_config(signer)
    }
}

impl From<L1SenderConfig> for zksync_os_l1_sender::config::L1SenderConfig<ExecuteCommand> {
    fn from(c: L1SenderConfig) -> Self {
        let signer = c
            .operator_execute_sk
            .clone()
            .expect("operator_execute_sk must be set on the Main Node");
        c.into_lib_l1_sender_config(signer)
    }
}

impl From<L1WatcherConfig> for zksync_os_l1_watcher::L1WatcherConfig {
    fn from(c: L1WatcherConfig) -> Self {
        Self {
            max_blocks_to_process: c.max_blocks_to_process,
            confirmations: c.confirmations,
            poll_interval: c.poll_interval,
        }
    }
}

impl From<MempoolConfig> for zksync_os_mempool::PoolConfig {
    fn from(c: MempoolConfig) -> Self {
        Self {
            pending_limit: SubPoolLimit::new(c.max_pending_txs, c.max_pending_size),
            minimal_protocol_basefee: c.minimal_protocol_basefee,
            ..Default::default()
        }
    }
}

impl From<MempoolTxValidatorConfig> for zksync_os_mempool::TxValidatorConfig {
    fn from(c: MempoolTxValidatorConfig) -> Self {
        Self {
            max_input_bytes: c.max_input_bytes,
        }
    }
}

impl From<RebuildBlocksConfig> for RebuildOptions {
    fn from(c: RebuildBlocksConfig) -> Self {
        Self {
            from_block: c.from_block,
            blocks_to_empty: c.blocks_to_empty.into_iter().collect(),
            reset_timestamps: c.reset_timestamps,
        }
    }
}

impl From<BatchVerificationConfig> for zksync_os_batch_verification::BatchVerificationConfig {
    fn from(c: BatchVerificationConfig) -> Self {
        Self {
            server_enabled: c.server_enabled,
            client_enabled: c.client_enabled,
            threshold: c.threshold,
            accepted_signers: c.accepted_signers,
            request_timeout: c.request_timeout,
            retry_delay: c.retry_delay,
            total_timeout: c.total_timeout,
            signing_key: c.signing_key,
        }
    }
}

pub fn gas_adjuster_config(
    c: GasAdjusterConfig,
    pubdata_mode: PubdataMode,
    max_priority_fee_per_gas_wei: u128,
) -> zksync_os_gas_adjuster::GasAdjusterConfig {
    zksync_os_gas_adjuster::GasAdjusterConfig {
        pubdata_mode,
        max_base_fee_samples: c.max_base_fee_samples,
        num_samples_for_blob_base_fee_estimate: c.num_samples_for_blob_base_fee_estimate,
        max_blob_fill_ratio_samples: c.max_blob_fill_ratio_samples,
        max_priority_fee_per_gas: max_priority_fee_per_gas_wei,
        poll_period: c.poll_period,
        pubdata_pricing_multiplier: c.pubdata_pricing_multiplier,
    }
}

pub fn base_token_price_updater_config(
    c: &BaseTokenPriceUpdaterConfig,
    l1_sender_config: &L1SenderConfig,
) -> zksync_os_base_token_adjuster::BaseTokenPriceUpdaterConfig {
    let token_multiplier_setter_signer = c.token_multiplier_setter_sk.clone();

    zksync_os_base_token_adjuster::BaseTokenPriceUpdaterConfig {
        price_polling_interval: c.price_polling_interval,
        l1_update_deviation_percentage: c.l1_update_deviation_percentage,
        price_fetching_max_attempts: c.price_fetching_max_attempts,
        base_token_addr_override: c.base_token_addr_override,
        base_token_decimals_override: c.base_token_decimals_override,
        gateway_base_token_addr_override: c.gateway_base_token_addr_override,
        token_multiplier_setter_signer,
        max_fee_per_gas_wei: l1_sender_config.max_fee_per_gas.0,
        max_priority_fee_per_gas_wei: l1_sender_config.max_priority_fee_per_gas.0,
        fallback_prices: c.fallback_prices.clone(),
    }
}

impl From<ForcedPriceClientConfig> for zksync_os_external_price_api::ForcedPriceClientConfig {
    fn from(c: ForcedPriceClientConfig) -> Self {
        Self {
            prices: c.prices,
            fluctuation: c.fluctuation,
            next_value_fluctuation: c.next_value_fluctuation,
        }
    }
}

impl From<ExternalPriceApiClientConfig>
    for zksync_os_external_price_api::ExternalPriceApiClientConfig
{
    fn from(c: ExternalPriceApiClientConfig) -> Self {
        match c {
            ExternalPriceApiClientConfig::Forced { forced } => Self::Forced {
                forced: forced.into(),
            },
            ExternalPriceApiClientConfig::CoinGecko {
                base_url,
                coingecko_api_key,
                client_timeout,
            } => Self::CoinGecko {
                base_url,
                coingecko_api_key,
                client_timeout,
            },
            ExternalPriceApiClientConfig::CoinMarketCap {
                base_url,
                cmc_api_key,
                client_timeout,
            } => Self::CoinMarketCap {
                base_url,
                cmc_api_key,
                client_timeout,
            },
        }
    }
}

impl From<FeeConfig> for zksync_os_sequencer::execution::FeeConfig {
    fn from(c: FeeConfig) -> Self {
        let native_price_usd = {
            let r = Ratio::<BigInt>::from_float(c.native_price_usd)
                .expect("Failed to convert native_price_usd to ratio");
            Ratio::new(
                r.numer().to_biguint().unwrap(),
                r.denom().to_biguint().unwrap(),
            )
        };

        Self {
            native_price_usd,
            base_fee_override: c.base_fee_override.map(|n| BigUint::from(n.to::<u128>())),
            native_per_gas: c.native_per_gas,
            pubdata_price_override: c
                .pubdata_price_override
                .map(|n| BigUint::from(n.to::<u128>())),
            pubdata_price_cap: c.pubdata_price_cap.map(|n| BigUint::from(n.to::<u128>())),
            native_price_override: c
                .native_price_override
                .map(|n| BigUint::from(n.to::<u128>())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::signers::k256::ecdsa::SigningKey;
    use smart_config::{ConfigRepository, ConfigSchema, DescribeConfig, Environment};
    use std::net::Ipv4Addr;

    const TEST_SECRET_KEY: &str =
        "0x1111111111111111111111111111111111111111111111111111111111111111";

    fn loopback_interface() -> &'static str {
        ["lo", "lo0"]
            .into_iter()
            .find(|interface| super::resolve_net_if_ip(interface).is_ok())
            .expect("expected a loopback interface")
    }

    fn parse_network_config<const N: usize>(env_vars: [(&str, &str); N]) -> NetworkConfig {
        let schema = ConfigSchema::new(&NetworkConfig::DESCRIPTION, "network");
        let repo = ConfigRepository::new(&schema).with(Environment::from_iter("", env_vars));
        repo.single::<NetworkConfig>().unwrap().parse().unwrap()
    }

    #[test]
    fn network_interface_is_a_separate_field_and_overrides_address() {
        let config = parse_network_config([
            ("NETWORK_SECRET_KEY", TEST_SECRET_KEY),
            ("NETWORK_ADDRESS", "10.0.0.1"),
            ("NETWORK_INTERFACE", loopback_interface()),
        ]);
        assert_eq!(config.address, Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(config.interface.as_deref(), Some(loopback_interface()));

        let runtime_config = zksync_os_network::config::NetworkConfig::from(config);
        assert_eq!(runtime_config.address, Ipv4Addr::LOCALHOST);
    }

    #[test]
    fn network_boot_nodes_accept_dns_names() {
        let config = parse_network_config([(
            "NETWORK_BOOT_NODES",
            "enode://6f8a80d14311c39f35f516fa664deaaaa13e85b2f7493f37f6144d86991ec012937307647bd3b9a82abe2974e1407241d54947bbb39763a4cac9f77166ad92a0@localhost:30303?discport=30301",
        )]);

        assert_eq!(config.boot_nodes.len(), 1);
        assert_eq!(config.boot_nodes[0].host.to_string(), "localhost");
        let record = config.boot_nodes[0].resolve_blocking().unwrap();
        assert!(record.address.is_loopback());
        assert_eq!(record.tcp_port, 30303);
        assert_eq!(record.udp_port, 30301);
    }

    #[test]
    #[should_panic(expected = "failed to resolve network interface 'definitely-missing-if'")]
    fn invalid_network_interface_panics() {
        let _ = zksync_os_network::config::NetworkConfig::from(parse_network_config([
            ("NETWORK_SECRET_KEY", TEST_SECRET_KEY),
            ("NETWORK_INTERFACE", "definitely-missing-if"),
            ("NETWORK_ADDRESS", "10.0.0.1"),
        ]));
    }

    fn local_signer(byte: u8) -> SignerConfig {
        SignerConfig::Local(SigningKey::from_slice(&[byte; 32]).unwrap())
    }

    fn base_config(node_role: NodeRole) -> Config {
        Config {
            general_config: GeneralConfig {
                node_role,
                run_priority_tree: true,
                ..Default::default()
            },
            network_config: NetworkConfig::default(),
            genesis_config: GenesisConfig {
                bridgehub_address: Some(Address::ZERO),
                bytecode_supplier_address: Some(Address::with_last_byte(0x01)),
                chain_id: Some(270),
                genesis_input_path: Some("genesis.json".into()),
            },
            rpc_config: RpcConfig::default(),
            mempool_config: MempoolConfig::default(),
            tx_validator_config: MempoolTxValidatorConfig::default(),
            sequencer_config: SequencerConfig::default(),
            l1_sender_config: L1SenderConfig {
                operator_commit_sk: Some(local_signer(0x11)),
                operator_prove_sk: Some(local_signer(0x22)),
                operator_execute_sk: Some(local_signer(0x33)),
                max_fee_per_gas: 200 * EtherUnit::Gwei,
                max_priority_fee_per_gas: 1 * EtherUnit::Gwei,
                max_fee_per_blob_gas: 2 * EtherUnit::Gwei,
                command_limit: 16,
                poll_interval: Duration::from_millis(100),
                transaction_timeout: Duration::from_secs(600),
                fusaka_upgrade_timestamp: u64::MAX,
                enabled: true,
                pubdata_mode: Some(PubdataMode::Blobs),
            },
            l1_watcher_config: L1WatcherConfig::default(),
            batcher_config: BatcherConfig::default(),
            prover_input_generator_config: ProverInputGeneratorConfig::default(),
            prover_api_config: ProverApiConfig::default(),
            status_server_config: StatusServerConfig::default(),
            observability_config: ObservabilityConfig::default(),
            gas_adjuster_config: GasAdjusterConfig::default(),
            batch_verification_config: BatchVerificationConfig::default(),
            base_token_price_updater_config: BaseTokenPriceUpdaterConfig::default(),
            interop_fee_updater_config: InteropFeeUpdaterConfig::default(),
            external_price_api_client_config: Some(ExternalPriceApiClientConfig::Forced {
                forced: ForcedPriceClientConfig::default(),
            }),
            fee_config: FeeConfig::default(),
        }
    }

    #[tokio::test]
    async fn main_node_validation_reports_all_main_only_required_fields() {
        let mut config = base_config(NodeRole::MainNode);
        config.genesis_config.bridgehub_address = None;
        config.genesis_config.bytecode_supplier_address = None;
        config.genesis_config.chain_id = None;
        config.genesis_config.genesis_input_path = None;
        config.l1_sender_config.operator_commit_sk = None;
        config.l1_sender_config.operator_prove_sk = None;
        config.l1_sender_config.operator_execute_sk = None;
        config.l1_sender_config.pubdata_mode = None;
        config.external_price_api_client_config = None;

        let err = config.validate().await.unwrap_err().to_string();

        assert!(
            err.contains("`genesis.bridgehub_address` is required when `general.node_role=main`")
        );
        assert!(err.contains(
            "`genesis.bytecode_supplier_address` is required when `general.node_role=main`"
        ));
        assert!(err.contains("`genesis.chain_id` is required when `general.node_role=main`"));
        assert!(
            err.contains("`genesis.genesis_input_path` is required when `general.node_role=main`")
        );
        assert!(
            err.contains(
                "`l1_sender.operator_commit_sk` is required when `general.node_role=main`"
            )
        );
        assert!(
            err.contains("`l1_sender.operator_prove_sk` is required when `general.node_role=main`")
        );
        assert!(
            err.contains(
                "`l1_sender.operator_execute_sk` is required when `general.node_role=main`"
            )
        );
        assert!(err.contains("`l1_sender.pubdata_mode` is required when `general.node_role=main`"));
        assert!(
            err.contains("`external_price_api_client` is required when `general.node_role=main`")
        );
    }

    #[tokio::test]
    async fn external_node_can_omit_main_only_fields() {
        let mut config = base_config(NodeRole::ExternalNode);
        config.general_config.main_node_rpc_url = Some("http://127.0.0.1:3050".into());
        config.network_config.enabled = true;
        config.network_config.secret_key = Some(SecretKey::from_slice(&[0x44; 32]).unwrap());
        config.genesis_config.bridgehub_address = None;
        config.genesis_config.bytecode_supplier_address = None;
        config.genesis_config.chain_id = None;
        config.genesis_config.genesis_input_path = None;
        config.l1_sender_config.operator_commit_sk = None;
        config.l1_sender_config.operator_prove_sk = None;
        config.l1_sender_config.operator_execute_sk = None;
        config.l1_sender_config.pubdata_mode = None;
        config.external_price_api_client_config = None;

        config.validate().await.unwrap();
    }

    #[tokio::test]
    async fn main_node_validation_rejects_duplicate_operator_addresses() {
        let mut config = base_config(NodeRole::MainNode);
        let signer = local_signer(0x55);
        config.l1_sender_config.operator_commit_sk = Some(signer.clone());
        config.l1_sender_config.operator_prove_sk = Some(signer.clone());
        config.l1_sender_config.operator_execute_sk = Some(local_signer(0x66));

        let err = config.validate().await.unwrap_err().to_string();

        assert!(err.contains("must be different"));
    }

    #[tokio::test]
    async fn main_node_validation_aggregates_sync_and_async_errors() {
        let mut config = base_config(NodeRole::MainNode);
        config.genesis_config.bridgehub_address = None;
        let signer = local_signer(0x77);
        config.l1_sender_config.operator_commit_sk = Some(signer.clone());
        config.l1_sender_config.operator_prove_sk = Some(signer);

        let err = config.validate().await.unwrap_err().to_string();

        assert!(
            err.contains("`genesis.bridgehub_address` is required when `general.node_role=main`")
        );
        assert!(err.contains("`l1_sender.operator_commit_sk`"));
        assert!(err.contains("must be different"));
    }

    #[tokio::test]
    async fn batch_verification_requires_networking() {
        let mut config = base_config(NodeRole::MainNode);
        config.batch_verification_config.server_enabled = true;
        config.batch_verification_config.client_enabled = true;
        config.network_config.enabled = false;

        let err = config.validate().await.unwrap_err().to_string();

        assert!(
            err.contains("`batch_verification.server_enabled` requires `network.enabled=true`")
        );
        assert!(
            err.contains("`batch_verification.client_enabled` requires `network.enabled=true`")
        );
    }
}
