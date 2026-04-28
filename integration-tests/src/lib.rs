use crate::config::{ChainLayout, load_chain_config};
use crate::dyn_wallet_provider::EthDynProvider;
use crate::network::Zksync;
use crate::node_log::NodeLogState;
use crate::prover_tester::ProverTester;
use crate::provider::{ZksyncApi, ZksyncTestingProvider};
use crate::rpc_recorder::{HttpRpcRecorder, RpcRecordConfig};
use crate::test_config::{build_node_config, disable_prover_input_generation};
use crate::utils::LockedPort;
use alloy::network::EthereumWallet;
use alloy::primitives::U256;
use alloy::providers::utils::Eip1559Estimator;
use alloy::providers::{
    DynProvider, Identity, PendingTransactionBuilder, Provider, ProviderBuilder, WalletProvider,
};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::{LocalSigner, PrivateKeySigner};
use anyhow::Context;
use backon::ConstantBuilder;
use backon::Retryable;
use reth_tasks::{PanickedTaskError, Runtime, RuntimeBuilder, RuntimeConfig};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tempfile::TempDir;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tracing::Instrument;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_network::NodeRecord;
use zksync_os_server::config::Config;
pub use zksync_os_server::config::DeploymentFilterConfig;
use zksync_os_server::default_protocol_version::{
    NEXT_PROTOCOL_VERSION, PROTOCOL_VERSION, PROTOCOL_VERSION_V31_0,
};
use zksync_os_state_full_diffs::FullDiffsState;
use zksync_os_types::{
    L1PriorityTxType, L1TxType, NodeRole, REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
};

pub mod assert_traits;
pub mod config;
pub mod contracts;
pub mod dyn_wallet_provider;
mod network;
mod node_log;
mod prover_tester;
pub mod provider;
pub mod rpc_recorder;
pub mod test_config;
pub mod upgrade;
mod utils;

/// L1 chain id as expected by contracts deployed in `l1-state.json.gz`
const L1_CHAIN_ID: u64 = 31337;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementLayer {
    L1,
    Gateway,
}

pub use zksync_os_integration_tests_macros::test_multisetup;

#[derive(Debug, Clone, Copy)]
pub struct TestCase {
    pub protocol_version: &'static str,
    pub settlement_layer: SettlementLayer,
}

impl TestCase {
    pub const fn current_to_l1() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            settlement_layer: SettlementLayer::L1,
        }
    }

    pub const fn next_to_gateway() -> Self {
        Self {
            protocol_version: NEXT_PROTOCOL_VERSION,
            settlement_layer: SettlementLayer::Gateway,
        }
    }

    pub async fn environment(self) -> anyhow::Result<TestEnvironment> {
        TestEnvironment::from_case(self).await
    }
}

pub const CURRENT_TO_L1: TestCase = TestCase::current_to_l1();
pub const NEXT_TO_GATEWAY: TestCase = TestCase::next_to_gateway();

/// Set of private keys for batch verification participants.
pub const BATCH_VERIFICATION_KEYS: [&str; 2] = [
    "0x7094f4b57ed88624583f68d2f241858f7dafb6d2558bc22d18991690d36b4e47",
    "0xf9306dd03807c08b646d47c739bd51e4d2a25b02bad0efb3d93f095982ac98cd",
];
/// Shutdown completes in <5 seconds when there is no CPU starvation. But because prover input
/// generator runs its CPU-bound task on a blocking thread it can significantly slow down graceful
/// shutdown. We put 60s here until zksync-os v0.4.0 which will get rid of RISC-V simulator and
/// allow async/abortable prover input generation.
const NODE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);
const PORT_ACQUISITION_TIMEOUT: Duration = Duration::from_secs(15);
const PORT_ACQUISITION_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Set of addresses (i.e. public keys) expected by batch verification. Derived from [`BATCH_VERIFICATION_KEYS`].
static BATCH_VERIFICATION_ADDRESSES: LazyLock<Vec<String>> = LazyLock::new(|| {
    BATCH_VERIFICATION_KEYS
        .map(|key| {
            PrivateKeySigner::from_str(key)
                .unwrap()
                .address()
                .to_string()
        })
        .to_vec()
});

pub struct TestEnvironment {
    l1: AnvilL1,
    chain_layout: ChainLayout<'static>,
    gateway: Option<GatewayContext>,
    prepared_runtime: PreparedRuntime,
}

struct PreparedRuntime {
    tempdir: Arc<TempDir>,
    ports: Ports,
}

struct GatewayContext {
    rpc_url: String,
    node: SupportingNode,
}

impl PreparedRuntime {
    async fn new() -> anyhow::Result<Self> {
        Ok(Self {
            tempdir: Arc::new(tempfile::tempdir()?),
            ports: Ports::acquire_unused().await?,
        })
    }
}

impl TestEnvironment {
    async fn from_case(case: TestCase) -> anyhow::Result<Self> {
        match case.settlement_layer {
            SettlementLayer::L1 => {
                let chain_layout = ChainLayout::Default {
                    protocol_version: case.protocol_version,
                };
                let l1 = AnvilL1::start(chain_layout).await?;
                let prepared_runtime = PreparedRuntime::new().await?;
                Ok(Self {
                    l1,
                    chain_layout,
                    gateway: None,
                    prepared_runtime,
                })
            }
            SettlementLayer::Gateway => {
                let protocol_version = case.protocol_version;
                let chain_layout = ChainLayout::GatewayChain {
                    protocol_version,
                    chain_index: 0,
                };
                let l1 = AnvilL1::start(ChainLayout::Gateway { protocol_version }).await?;
                let mut gateway_config =
                    build_node_config(&l1, ChainLayout::Gateway { protocol_version }).await?;
                if !prover_input_generation_enabled() {
                    disable_prover_input_generation(&mut gateway_config);
                }
                let gateway = Tester::launch_with_new_runtime(
                    l1.clone(),
                    ChainLayout::Gateway { protocol_version },
                    gateway_config,
                )
                .await?;
                let gateway = GatewayContext::from_tester(gateway);
                let prepared_runtime = PreparedRuntime::new().await?;
                Ok(Self {
                    l1,
                    chain_layout,
                    gateway: Some(gateway),
                    prepared_runtime,
                })
            }
        }
    }

    pub async fn default_config(&self) -> anyhow::Result<Config> {
        let mut config = build_node_config(&self.l1, self.chain_layout).await?;
        if let Some(gateway) = &self.gateway {
            config.general_config.gateway_rpc_url = Some(gateway.rpc_url.clone());
        }
        Tester::bind_runtime_config(
            &self.l1,
            self.prepared_runtime.tempdir.as_ref(),
            &mut config,
            &self.prepared_runtime.ports,
        );
        Ok(config)
    }

    pub async fn launch_default(self) -> anyhow::Result<Tester> {
        let config = self.default_config().await?;
        self.launch(config).await
    }

    pub async fn launch(mut self, mut config: Config) -> anyhow::Result<Tester> {
        if !prover_input_generation_enabled() {
            disable_prover_input_generation(&mut config);
        }
        Tester::bind_runtime_config(
            &self.l1,
            self.prepared_runtime.tempdir.as_ref(),
            &mut config,
            &self.prepared_runtime.ports,
        );
        let supporting_gateway = if let Some(gateway) = self.gateway.take() {
            config
                .general_config
                .gateway_rpc_url
                .get_or_insert_with(|| gateway.rpc_url.clone());
            wait_for_gateway_readiness(&self.l1, &gateway.rpc_url, &config).await?;
            Some(gateway.node)
        } else {
            None
        };
        let mut tester = Tester::launch_node_inner(
            self.l1,
            config,
            self.prepared_runtime.tempdir,
            self.chain_layout,
            None,
            true,
            Some(self.prepared_runtime.ports),
        )
        .await?;
        if let Some(gateway) = supporting_gateway {
            tester.owned_supporting_nodes.push(gateway);
        }
        Ok(tester)
    }
}

/// A running primary test node together with its effective config, clients and any supporting
/// runtimes that need to stay alive for the test topology.
#[derive(Debug)]
pub struct Tester {
    l1: AnvilL1,
    pub l2_provider: EthDynProvider,
    /// ZKsync OS-specific provider. Generally prefer to use `l2_provider` as we strive for the
    /// system to be Ethereum-compatible. But this can be useful if you need to assert custom fields
    /// that are only present in ZKsync OS response types (`l2ToL1Logs`, `commitTx`, etc).
    pub l2_zk_provider: DynProvider<Zksync>,
    pub l2_wallet: EthereumWallet,

    pub prover_tester: ProverTester,

    runtime: Runtime,
    task_manager_handle: Option<JoinHandle<Result<(), PanickedTaskError>>>,
    config: Config,

    #[allow(dead_code)]
    tempdir: Arc<tempfile::TempDir>,

    // Needed to be able to connect external nodes
    node_record: NodeRecord,
    l2_rpc_address: String,
    #[expect(dead_code)]
    status_server_url: String,
    gateway_rpc_url: Option<String>,
    sl_provider: EthDynProvider,
    log_state: NodeLogState,
    chain_layout: ChainLayout<'static>,
    owned_supporting_nodes: Vec<SupportingNode>,
}

/// A stopped test node that keeps its database, effective config and L1 alive so it can be
/// started again.
#[derive(Debug)]
pub struct StoppedTester {
    l1: AnvilL1,
    config: Config,
    tempdir: Arc<tempfile::TempDir>,
    log_state: NodeLogState,
    chain_layout: ChainLayout<'static>,
    owned_supporting_nodes: Vec<SupportingNode>,
}

#[derive(Debug)]
struct SupportingNode {
    runtime: Runtime,
    _tempdir: Arc<TempDir>,
}

struct Ports {
    l2_rpc: LockedPort,
    prover_api: LockedPort,
    network: LockedPort,
    status: LockedPort,
}

impl Tester {
    pub fn config(&self) -> &Config {
        &self.config
    }

    fn apply_external_node_defaults(&self, config: &mut Config) {
        config.general_config.node_role = NodeRole::ExternalNode;
        config.network_config.boot_nodes = vec![self.node_record.into()];
        config.general_config.main_node_rpc_url = Some(self.l2_rpc_address.clone());
        config.general_config.gateway_rpc_url = self.gateway_rpc_url.clone();
        config.prover_api_config.fake_fri_provers.enabled = true;
        config.prover_api_config.fake_snark_provers.enabled = true;
        config.prover_input_generator_config.logging_enabled = false;
        config.batch_verification_config.server_enabled = false;
        config.l1_sender_config.pubdata_mode = None;
    }

    pub fn l1_provider(&self) -> &EthDynProvider {
        &self.l1.provider
    }

    pub fn l1_wallet(&self) -> &EthereumWallet {
        &self.l1.wallet
    }

    pub fn sl_provider(&self) -> &EthDynProvider {
        &self.sl_provider
    }

    /// Returns the gateway provider if a gateway RPC URL is configured, `None` otherwise.
    /// Use this when calling [`L1State::fetch`] or [`L1State::fetch_finalized`].
    pub fn gateway_eth_provider(&self) -> Option<DynProvider> {
        self.gateway_rpc_url
            .as_ref()
            .map(|_| self.sl_provider.clone().erased())
    }

    pub async fn gateway_provider(&self) -> anyhow::Result<Option<DynProvider<Zksync>>> {
        let provider: Option<DynProvider<Zksync>> =
            if let Some(gateway_rpc_url) = &self.gateway_rpc_url {
                Some(DynProvider::<Zksync>::new(
                    ProviderBuilder::<Identity, Identity, Zksync>::default()
                        .with_recommended_fillers()
                        .connect(gateway_rpc_url)
                        .await
                        .with_context(|| {
                            format!("failed to connect to gateway RPC at {gateway_rpc_url}")
                        })?,
                ))
            } else {
                None
            };
        Ok(provider)
    }

    /// Waits until the node reports a fatal critical-task error through the runtime task manager.
    ///
    /// This consumes the runtime's task manager handle for this tester instance, so it should be
    /// used only in tests that expect the node to fail.
    pub async fn wait_for_fatal_error_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> anyhow::Result<PanickedTaskError> {
        let task_manager_handle = self
            .task_manager_handle
            .take()
            .context("task manager handle was already taken")?;

        let result = tokio::time::timeout(timeout, task_manager_handle)
            .await
            .context("timed out waiting for fatal node error")?;

        match result {
            Ok(Err(err)) => Ok(err),
            Ok(Ok(())) => anyhow::bail!("node shut down gracefully before any fatal error"),
            Err(err) => Err(anyhow::Error::new(err).context("task manager join failed")),
        }
    }
}

impl Tester {
    pub async fn setup() -> anyhow::Result<Self> {
        CURRENT_TO_L1.environment().await?.launch_default().await
    }

    pub fn l2_rpc_url(&self) -> &str {
        &self.l2_rpc_address
    }

    pub fn record_l2_http_rpc(&self, config: RpcRecordConfig) -> HttpRpcRecorder {
        HttpRpcRecorder::start_http("l2", self.l2_rpc_url(), config)
    }

    pub fn external_node_config(&self) -> Config {
        let mut config = self.config.clone();
        self.apply_external_node_defaults(&mut config);
        config
    }

    pub async fn wait_for_initial_deposit(&self) -> anyhow::Result<()> {
        tokio::time::timeout(
            Duration::from_secs(60),
            self.l2_zk_provider.wait_for_block(2),
        )
        .await
        .context("timed out waiting for block 2 (initial deposit)")??;
        ensure_test_wallet_funded(
            &self.l1,
            &self.l2_provider,
            &self.l2_zk_provider,
            &self.l2_wallet,
        )
        .await
    }

    pub async fn launch_external_node(&self) -> anyhow::Result<Self> {
        // Due to type inference issue, we need to specify None type here and this whole function if a de-facto helper for this
        self.launch_external_node_inner(None::<fn(&mut Config)>)
            .await
    }

    pub async fn launch_external_node_overrides(
        &self,
        config_overrides: impl FnOnce(&mut Config),
    ) -> anyhow::Result<Self> {
        self.launch_external_node_inner(Some(config_overrides))
            .await
    }

    async fn launch_external_node_inner(
        &self,
        config_overrides: Option<impl FnOnce(&mut Config)>,
    ) -> anyhow::Result<Self> {
        let mut config = self.external_node_config();
        if let Some(config_overrides) = config_overrides {
            config_overrides(&mut config);
        }
        self.launch_from_config(config).await
    }

    pub async fn launch_from_config(&self, config: Config) -> anyhow::Result<Self> {
        Self::launch_with_new_runtime(self.l1.clone(), self.chain_layout, config).await
    }

    /// Gracefully shut down and restart the node, reusing the same database and L1.
    ///
    /// Returns a new `Tester` connected to the restarted node. The original `Tester` is consumed.
    ///
    /// Restart keeps the same config by default, including the original ports.
    pub async fn stop(self) -> anyhow::Result<StoppedTester> {
        let Self {
            runtime,
            l1,
            config,
            tempdir,
            log_state,
            chain_layout,
            owned_supporting_nodes,
            ..
        } = self;
        shutdown_runtime(runtime).await?;
        Ok(StoppedTester {
            l1,
            tempdir,
            log_state,
            chain_layout,
            config,
            owned_supporting_nodes,
        })
    }

    /// Restart keeps the same config by default. The internal P2P network port may change.
    pub async fn restart(self) -> anyhow::Result<Self> {
        self.stop().await?.start().await
    }

    pub async fn restart_with_config(self, config: Config) -> anyhow::Result<Self> {
        self.stop().await?.start_with_config(config).await
    }

    /// Gracefully shut down and restart the node, reusing the same database and L1,
    /// while applying additional config overrides for the restarted node.
    pub async fn restart_with_overrides(
        self,
        config_overrides: impl FnOnce(&mut Config),
    ) -> anyhow::Result<Self> {
        self.stop()
            .await?
            .start_with_overrides(config_overrides)
            .await
    }

    /// Gracefully shut down the node.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        let Self {
            runtime,
            owned_supporting_nodes,
            ..
        } = self;
        drop(owned_supporting_nodes);
        shutdown_runtime(runtime).await?;
        Ok(())
    }

    async fn launch_with_new_runtime(
        l1: AnvilL1,
        chain_layout: ChainLayout<'static>,
        mut config: Config,
    ) -> anyhow::Result<Self> {
        let tempdir = Arc::new(tempfile::tempdir()?);
        let ports = Ports::acquire_unused().await?;
        Self::bind_runtime_config(&l1, tempdir.as_ref(), &mut config, &ports);
        Self::launch_node_inner(l1, config, tempdir, chain_layout, None, true, Some(ports)).await
    }

    fn bind_runtime_config(l1: &AnvilL1, tempdir: &TempDir, config: &mut Config, ports: &Ports) {
        config.general_config.rocks_db_path = tempdir.path().join("rocksdb");
        config.general_config.l1_rpc_url = l1.address.clone();
        config.rpc_config.address = format!("0.0.0.0:{}", ports.l2_rpc.port);
        config.prover_api_config.address = format!("0.0.0.0:{}", ports.prover_api.port);
        config.prover_api_config.proof_storage.path = tempdir.path().join("proof_storage_path");
        config.status_server_config.address = format!("0.0.0.0:{}", ports.status.port);
        config.network_config.address = Ipv4Addr::LOCALHOST;
        config.network_config.interface = None;
        config.network_config.port = ports.network.port;
        config.network_config.secret_key = Some(zksync_os_network::rng_secret_key());
    }

    async fn launch_node_inner(
        l1: AnvilL1,
        config: Config,
        tempdir: Arc<TempDir>,
        chain_layout: ChainLayout<'static>,
        log_state: Option<NodeLogState>,
        wait_for_initial_deposit: bool,
        held_ports: Option<Ports>,
    ) -> anyhow::Result<Self> {
        let _ports = match held_ports {
            Some(ports) => ports,
            None => Ports::from_config(&config).await?,
        };
        #[cfg(feature = "prover-tests")]
        let enable_prover = !config.prover_api_config.fake_fri_provers.enabled;
        let l2_rpc_address = config.rpc_config.address.clone();
        let l2_rpc_ws_url = format!("ws://localhost:{}", parse_local_port(&l2_rpc_address)?);
        let status_server_url = config
            .status_server_config
            .address
            .replace("0.0.0.0:", "http://localhost:");

        let network_secret_key = config
            .network_config
            .secret_key
            .as_ref()
            .context("network secret key should be present in test config")?;
        let node_record = NodeRecord::from_secret_key(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), config.network_config.port),
            network_secret_key,
        );

        if let Some(ephemeral_state) = &config.general_config.ephemeral_state {
            tracing::info!("Loading ephemeral state from {}", ephemeral_state.display());
            zksync_os_server::util::unpack_ephemeral_state(
                ephemeral_state,
                &config.general_config.rocks_db_path,
            );
        }
        let node_role = config.general_config.node_role;
        let log_state = log_state.unwrap_or_else(|| NodeLogState::fresh(node_role));
        let log_tag = log_state.tag();
        let gateway_rpc_url = config.general_config.gateway_rpc_url.clone();
        #[cfg(feature = "prover-tests")]
        let prover_api_address = config.prover_api_config.address.clone();

        let runtime = RuntimeBuilder::new(RuntimeConfig::with_existing_handle(Handle::current()))
            .build()
            .expect("failed to build runtime");
        let node_span = tracing::info_span!(
            "node",
            node = %log_tag,
            role = %node_role,
        );
        tracing::info!(parent: &node_span, "Launching test node");
        zksync_os_server::run::<FullDiffsState>(&runtime, config.clone())
            .instrument(node_span)
            .await;
        let task_manager_handle = runtime
            .take_task_manager_handle()
            .expect("Runtime must contain a TaskManager handle");

        #[cfg(feature = "prover-tests")]
        if enable_prover {
            let base_url = prover_api_address.replace("0.0.0.0:", "http://localhost:");
            let app_bin_path = utils::materialize_multiblock_batch_bin(
                &tempdir.path().join("app_bins"),
                "v6",
                zksync_os_multivm::apps::v6::MULTIBLOCK_BATCH,
            );
            let trusted_setup_file = std::env::var("COMPACT_CRS_FILE").unwrap();
            let output_dir = tempdir.path().join("outputs");
            std::fs::create_dir_all(&output_dir).unwrap();

            let path = download_prover_and_unpack(cfg!(feature = "gpu-prover-tests")).await;

            let mut child = tokio::process::Command::new(path)
                .arg("--sequencer-urls")
                .arg(base_url)
                .arg("--app-bin-path")
                .arg(app_bin_path)
                .arg("--circuit-limit")
                .arg("10000")
                .arg("--output-dir")
                .arg(output_dir)
                .arg("--trusted-setup-file")
                .arg(trusted_setup_file)
                .arg("--iterations")
                .arg("1")
                .arg("--max-fris-per-snark")
                .arg("1")
                .arg("--disable-zk")
                .spawn()
                .expect("failed to spawn prover service");
            tokio::task::spawn(async move {
                let code = child
                    .wait()
                    .await
                    .expect("failed to wait for prover service");
                if code.success() {
                    tracing::info!("prover service finished running");
                } else {
                    panic!("prover service terminated with exit code {}", code);
                }
            });
        }

        let l2_wallet = EthereumWallet::new(
            // Private key for 0x36615cf349d7f6344891b1e7ca7c72883f5dc049
            LocalSigner::from_str(
                "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110",
            )
            .unwrap(),
        );
        let l2_provider = (|| async {
            let l2_provider = ProviderBuilder::new()
                .wallet(l2_wallet.clone())
                .connect(&l2_rpc_ws_url)
                .await?;

            // Wait for L2 node to get up and be able to respond.
            l2_provider.get_chain_id().await?;
            anyhow::Ok(l2_provider)
        })
        .retry(
            ConstantBuilder::default()
                .with_delay(Duration::from_millis(200))
                .with_max_times(50),
        )
        .notify(|err: &anyhow::Error, dur: Duration| {
            tracing::info!(%err, ?dur, "retrying connection to L2 node");
        })
        .await?;

        let l2_zk_provider = ProviderBuilder::new_with_network::<Zksync>()
            .wallet(l2_wallet.clone())
            .connect(&l2_rpc_ws_url)
            .await?;

        let sl_provider = if let Some(gateway_rpc_url) = &gateway_rpc_url {
            let sl_provider = (|| async {
                let sl_provider = ProviderBuilder::new()
                    .wallet(l2_wallet.clone())
                    .connect(gateway_rpc_url)
                    .await?;

                // Wait for L2 node to get up and be able to respond.
                sl_provider.get_chain_id().await?;
                anyhow::Ok(sl_provider)
            })
            .retry(
                ConstantBuilder::default()
                    .with_delay(Duration::from_millis(200))
                    .with_max_times(50),
            )
            .notify(|err: &anyhow::Error, dur: Duration| {
                tracing::info!(%err, ?dur, "retrying connection to L2 node");
            })
            .await?;
            EthDynProvider::new(sl_provider)
        } else {
            l1.provider.clone()
        };
        let gateway_eth_provider = gateway_rpc_url.as_ref().map(|_| sl_provider.clone());
        let prover_tester = ProverTester::new(
            EthDynProvider::new(l1.provider.clone()),
            gateway_eth_provider,
            EthDynProvider::new(l2_provider.clone()),
            DynProvider::new(l2_zk_provider.clone()),
        );
        let tester = Tester {
            l1,
            l2_provider: EthDynProvider::new(l2_provider.clone()),
            l2_zk_provider: DynProvider::new(l2_zk_provider.clone()),
            l2_wallet,
            prover_tester,
            runtime,
            task_manager_handle: Some(task_manager_handle),
            config,
            l2_rpc_address: l2_rpc_address.replace("0.0.0.0:", "http://localhost:"),
            status_server_url,
            gateway_rpc_url,
            sl_provider,
            node_record,
            log_state,
            tempdir: tempdir.clone(),
            chain_layout,
            owned_supporting_nodes: Vec::new(),
        };
        if wait_for_initial_deposit {
            tester.wait_for_initial_deposit().await?;
        }
        Ok(tester)
    }
}

impl StoppedTester {
    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn l1_provider(&self) -> &EthDynProvider {
        &self.l1.provider
    }

    pub fn chain_layout(&self) -> ChainLayout<'static> {
        self.chain_layout
    }

    pub async fn shutdown(self) -> anyhow::Result<()> {
        drop(self.owned_supporting_nodes);
        Ok(())
    }

    pub async fn start(self) -> anyhow::Result<Tester> {
        let config = self.config.clone();
        self.start_with_config(config).await
    }

    pub async fn start_with_config(self, config: Config) -> anyhow::Result<Tester> {
        let Self {
            l1,
            tempdir,
            chain_layout,
            log_state,
            owned_supporting_nodes,
            ..
        } = self;
        let ports = Ports::from_config(&config).await?;
        let mut tester = Tester::launch_node_inner(
            l1,
            config,
            tempdir,
            chain_layout,
            Some(log_state.restarted()),
            false,
            Some(ports),
        )
        .await?;
        tester.owned_supporting_nodes = owned_supporting_nodes;
        Ok(tester)
    }

    pub async fn start_with_overrides(
        self,
        config_overrides: impl FnOnce(&mut Config),
    ) -> anyhow::Result<Tester> {
        let mut config = self.config.clone();
        config_overrides(&mut config);
        self.start_with_config(config).await
    }
}

impl SupportingNode {
    fn from_tester(tester: Tester) -> Self {
        let Tester {
            runtime,
            tempdir,
            owned_supporting_nodes,
            ..
        } = tester;
        drop(owned_supporting_nodes);
        Self {
            runtime,
            _tempdir: tempdir,
        }
    }
}

impl GatewayContext {
    fn from_tester(tester: Tester) -> Self {
        let rpc_url = tester.l2_rpc_url().to_owned();
        Self {
            rpc_url,
            node: SupportingNode::from_tester(tester),
        }
    }
}

impl Drop for SupportingNode {
    fn drop(&mut self) {
        let _ = self
            .runtime
            .graceful_shutdown_with_timeout(NODE_SHUTDOWN_TIMEOUT);
    }
}

impl Ports {
    async fn acquire_unused() -> anyhow::Result<Self> {
        Ok(Self {
            l2_rpc: LockedPort::acquire_unused().await?,
            prover_api: LockedPort::acquire_unused().await?,
            network: LockedPort::acquire_unused().await?,
            status: LockedPort::acquire_unused().await?,
        })
    }

    async fn from_config(config: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            l2_rpc: acquire_port_with_retry(parse_local_port(&config.rpc_config.address)?)
                .await
                .with_context(|| {
                    format!(
                        "failed to acquire L2 RPC port {}",
                        config.rpc_config.address
                    )
                })?,
            prover_api: acquire_port_with_retry(parse_local_port(
                &config.prover_api_config.address,
            )?)
            .await
            .with_context(|| {
                format!(
                    "failed to acquire prover API port {}",
                    config.prover_api_config.address
                )
            })?,
            network: acquire_port_with_retry(config.network_config.port)
                .await
                .with_context(|| {
                    format!(
                        "failed to acquire network port {}",
                        config.network_config.port
                    )
                })?,
            status: acquire_port_with_retry(parse_local_port(
                &config.status_server_config.address,
            )?)
            .await
            .with_context(|| {
                format!(
                    "failed to acquire status server port {}",
                    config.status_server_config.address
                )
            })?,
        })
    }
}

fn parse_local_port(address: &str) -> anyhow::Result<u16> {
    let port = address
        .rsplit_once(':')
        .context("address should contain a port")?
        .1;
    port.parse().context("address port should be numeric")
}

async fn acquire_port_with_retry(port: u16) -> anyhow::Result<LockedPort> {
    let deadline = tokio::time::Instant::now() + PORT_ACQUISITION_TIMEOUT;
    loop {
        match LockedPort::acquire(port).await {
            Ok(locked_port) => return Ok(locked_port),
            Err(err) if tokio::time::Instant::now() < deadline => {
                tracing::info!(port, %err, "retrying port acquisition");
                tokio::time::sleep(PORT_ACQUISITION_POLL_INTERVAL).await;
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "port {port} did not become acquirable within {PORT_ACQUISITION_TIMEOUT:?}"
                    )
                });
            }
        }
    }
}

async fn shutdown_runtime(runtime: Runtime) -> anyhow::Result<()> {
    let shutdown_ok = tokio::task::spawn_blocking(move || {
        runtime.graceful_shutdown_with_timeout(NODE_SHUTDOWN_TIMEOUT)
    })
    .await
    .expect("failed to join graceful shutdown task");
    if !shutdown_ok {
        panic!("node failed to shutdown in time");
    }
    Ok(())
}

async fn ensure_test_wallet_funded(
    l1: &AnvilL1,
    l2_provider: &EthDynProvider,
    l2_zk_provider: &DynProvider<Zksync>,
    l2_wallet: &EthereumWallet,
) -> anyhow::Result<()> {
    let beneficiary = l2_wallet.default_signer().address();
    let balance = l2_provider.get_balance(beneficiary).await?;
    if balance > U256::ZERO {
        return Ok(());
    }

    let chain_id = l2_provider.get_chain_id().await?;
    let bridgehub = Bridgehub::new(
        l2_zk_provider.get_bridgehub_contract().await?,
        l1.provider.clone(),
        chain_id,
    );
    let amount = U256::from(1_000_000_000_000_000_000u128) * U256::from(1_000u64);
    let max_priority_fee_per_gas = l1.provider.get_max_priority_fee_per_gas().await?;
    let base_l1_fees = l1
        .provider
        .estimate_eip1559_fees_with(Eip1559Estimator::new(|base_fee_per_gas, _| {
            alloy::eips::eip1559::Eip1559Estimation {
                max_fee_per_gas: base_fee_per_gas * 3 / 2,
                max_priority_fee_per_gas: 0,
            }
        }))
        .await?;
    let max_fee_per_gas = base_l1_fees.max_fee_per_gas + max_priority_fee_per_gas;
    let gas_limit = l2_provider
        .estimate_gas(
            TransactionRequest::default()
                .transaction_type(L1PriorityTxType::TX_TYPE)
                .from(beneficiary)
                .to(beneficiary)
                .value(amount),
        )
        .await?;
    let tx_base_cost = bridgehub
        .l2_transaction_base_cost(
            max_fee_per_gas + max_priority_fee_per_gas,
            gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
        )
        .await?;

    let receipt = l1
        .provider
        .send_transaction(
            bridgehub
                .request_l2_transaction_direct(
                    amount + tx_base_cost,
                    beneficiary,
                    amount,
                    vec![],
                    gas_limit,
                    REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
                    beneficiary,
                )
                .value(amount + tx_base_cost)
                .max_fee_per_gas(max_fee_per_gas)
                .max_priority_fee_per_gas(max_priority_fee_per_gas)
                .into_transaction_request(),
        )
        .await?
        .get_receipt()
        .await?;
    let l1_to_l2_tx_log = receipt
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<NewPriorityRequest>().ok())
        .next()
        .expect("no L1->L2 logs produced by funding tx");
    let l2_tx_hash = l1_to_l2_tx_log.inner.txHash;

    PendingTransactionBuilder::new(l2_zk_provider.root().clone(), l2_tx_hash)
        .get_receipt()
        .await?;

    (|| async {
        let balance = l2_provider.get_balance(beneficiary).await?;
        if balance > U256::ZERO {
            Ok(())
        } else {
            anyhow::bail!("L2 wallet is still unfunded")
        }
    })
    .retry(
        ConstantBuilder::default()
            .with_delay(Duration::from_secs(1))
            .with_max_times(10),
    )
    .await
}

/// Multi-node owner for gateway-settling tests.
///
/// Owns one gateway tester plus one tester per settling chain.
pub struct GatewayTester {
    gateway: Tester,
    chains: Vec<Tester>,
}

impl GatewayTester {
    pub fn builder() -> GatewayTesterBuilder {
        GatewayTesterBuilder::default()
    }

    pub async fn setup(num_chains: usize) -> anyhow::Result<Self> {
        Self::builder().num_chains(num_chains).build().await
    }

    /// Get a specific chain by index
    pub fn chain(&self, index: usize) -> &Tester {
        &self.chains[index]
    }

    pub fn chain_mut(&mut self, index: usize) -> &mut Tester {
        &mut self.chains[index]
    }

    pub fn gateway(&self) -> &Tester {
        &self.gateway
    }
}

pub struct GatewayTesterBuilder {
    protocol_version: &'static str,
    num_chains: Option<usize>,
    deployment_filter: Option<DeploymentFilterConfig>,
}

impl Default for GatewayTesterBuilder {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION_V31_0,
            num_chains: None,
            deployment_filter: None,
        }
    }
}

impl GatewayTesterBuilder {
    pub fn protocol_version(mut self, protocol_version: &'static str) -> Self {
        self.protocol_version = protocol_version;
        self
    }

    pub fn num_chains(mut self, num_chains: usize) -> Self {
        self.num_chains = Some(num_chains);
        self
    }

    /// Set the deployment filter config for all chains.
    pub fn deployment_filter(mut self, config: DeploymentFilterConfig) -> Self {
        self.deployment_filter = Some(config);
        self
    }

    pub async fn build(self) -> anyhow::Result<GatewayTester> {
        let num_chains = self.num_chains.unwrap_or(2);

        let protocol_version = self.protocol_version;
        let l1 = AnvilL1::start(ChainLayout::Gateway { protocol_version }).await?;
        let mut gateway_config =
            build_node_config(&l1, ChainLayout::Gateway { protocol_version }).await?;
        if !prover_input_generation_enabled() {
            disable_prover_input_generation(&mut gateway_config);
        }
        let gateway = Tester::launch_with_new_runtime(
            l1.clone(),
            ChainLayout::Gateway { protocol_version },
            gateway_config,
        )
        .await?;
        let gateway_rpc_url = gateway.l2_rpc_url().to_owned();

        let mut chains = Vec::with_capacity(num_chains);
        for i in 0..num_chains {
            let chain_layout = ChainLayout::GatewayChain {
                protocol_version,
                chain_index: i,
            };
            let chain_config = load_chain_config(chain_layout).await;
            let chain_id = chain_config
                .genesis_config
                .chain_id
                .expect("Chain ID must be set in chain config");
            wait_for_gateway_readiness(&l1, gateway.l2_rpc_url(), &chain_config).await?;
            let gateway_rpc_url = gateway_rpc_url.clone();
            let deployment_filter = self.deployment_filter.clone();

            let mut tester_config = build_node_config(&l1, chain_layout).await?;
            if !prover_input_generation_enabled() {
                disable_prover_input_generation(&mut tester_config);
            }
            tester_config.general_config.gateway_rpc_url = Some(gateway_rpc_url);
            if let Some(deployment_filter) = deployment_filter {
                tester_config
                    .sequencer_config
                    .tx_validator
                    .deployment_filter = deployment_filter;
            }

            let tester =
                Tester::launch_with_new_runtime(l1.clone(), chain_layout, tester_config).await?;

            tracing::info!(
                "L2 chain {} started with chain_id {} on {}",
                i,
                chain_id,
                tester.l2_rpc_address
            );

            chains.push(tester);
        }

        Ok(GatewayTester { gateway, chains })
    }
}

fn prover_input_generation_enabled() -> bool {
    std::env::var("NEXTEST_PROFILE").as_deref() != Ok("no-pig")
}

async fn wait_for_gateway_readiness(
    l1: &AnvilL1,
    gateway_rpc_url: &str,
    chain_config: &Config,
) -> anyhow::Result<()> {
    let chain_id = chain_config
        .genesis_config
        .chain_id
        .context("chain config is missing genesis chain_id")?;
    let bridgehub_address = chain_config
        .genesis_config
        .bridgehub_address
        .context("chain config is missing bridgehub_address")?;

    (|| async {
        let gateway_provider = ProviderBuilder::new()
            .connect(gateway_rpc_url)
            .await
            .with_context(|| format!("failed to connect to gateway RPC at {gateway_rpc_url}"))?;

        L1State::fetch_finalized(
            DynProvider::new(l1.provider.clone()),
            Some(DynProvider::new(gateway_provider)),
            bridgehub_address,
            chain_id,
        )
        .await
        .with_context(|| format!("gateway is not ready for chain {chain_id}"))?;
        anyhow::Ok(())
    })
    .retry(
        ConstantBuilder::default()
            .with_delay(Duration::from_millis(200))
            .with_max_times(300),
    )
    .notify(|err: &anyhow::Error, dur: Duration| {
        tracing::info!(chain_id, %err, ?dur, "retrying gateway readiness check");
    })
    .await
}

#[derive(Debug, Clone)]
pub struct AnvilL1 {
    pub address: String,
    pub provider: EthDynProvider,
    pub wallet: EthereumWallet,

    // Temporary directory that holds uncompressed l1-state.json used to initialize Anvil's state.
    // Needs to be held for the duration of test's lifetime.
    _tempdir: Arc<TempDir>,
}

impl AnvilL1 {
    async fn start(chain_layout: ChainLayout<'_>) -> anyhow::Result<Self> {
        let tempdir = tempfile::tempdir()?;
        let l1_state = chain_layout.l1_state();
        let l1_state_path = tempdir.path().join("l1-state.json");
        std::fs::write(&l1_state_path, &l1_state)
            .context("failed to write L1 state to temporary state file")?;

        let locked_port = LockedPort::acquire_unused().await?;
        let address = format!("http://localhost:{}", locked_port.port);

        let provider = ProviderBuilder::new().connect_anvil_with_wallet_and_config(|anvil| {
            anvil
                .port(locked_port.port)
                .chain_id(L1_CHAIN_ID)
                .arg("--block-time")
                .arg("0.25")
                .arg("--mixed-mining")
                .arg("--load-state")
                .arg(l1_state_path)
        })?;

        let wallet = provider.wallet().clone();

        (|| async {
            // Wait for L1 node to get up and be able to respond.
            provider.clone().get_chain_id().await?;
            Ok(())
        })
        .retry(
            ConstantBuilder::default()
                .with_delay(Duration::from_millis(200))
                .with_max_times(50),
        )
        .notify(|err: &anyhow::Error, dur: Duration| {
            tracing::info!(%err, ?dur, "retrying connection to L1 node");
        })
        .await?;

        tracing::info!("L1 chain started on {}", address);

        Ok(Self {
            address,
            provider: EthDynProvider::new(provider),
            wallet,
            _tempdir: Arc::new(tempdir),
        })
    }
}

#[cfg(feature = "prover-tests")]
async fn download_prover_and_unpack(gpu: bool) -> String {
    const RELEASE_VERSION: &str = "v0.7.1";
    const RELEASE_BASE_URL: &str =
        "https://github.com/matter-labs/zksync-airbender-prover/releases/download/v0.7.1";

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let asset_name = match (os, arch, gpu) {
        ("linux", "x86_64", true) => {
            "zksync-os-prover-service-v0.7.1-x86_64-unknown-linux-gnu-gpu.tar.gz"
        }
        ("linux", "x86_64", false) => {
            "zksync-os-prover-service-v0.7.1-x86_64-unknown-linux-gnu-cpu.tar.gz"
        }
        ("macos", _, true) => {
            panic!("GPU prover binary is not available for macOS in {RELEASE_VERSION}")
        }
        ("macos", _, false) => "zksync-os-prover-service-v0.7.1-universal-apple-darwin-cpu.tar.gz",
        ("linux", _, _) => panic!(
            "unsupported Linux architecture `{arch}` for prover binaries; supported architecture: x86_64"
        ),
        _ => panic!(
            "unsupported platform `{os}-{arch}` for prover binaries; supported platforms: linux-x86_64 (cpu/gpu), macos-* (cpu)"
        ),
    };

    let local_binary_name = asset_name.trim_end_matches(".tar.gz");
    let dir = std::path::Path::new("prover-binaries");
    if !std::fs::exists(dir).expect("failed to check dir existence") {
        std::fs::create_dir_all(dir).expect("failed to create dir");
    }

    let binary_path = dir.join(local_binary_name);
    if std::fs::exists(binary_path.as_path()).expect("failed to check binary existence") {
        tracing::info!(
            "prover service binary is already present at {}",
            binary_path.display()
        );
        return binary_path.display().to_string();
    }

    let archive_path = dir.join(asset_name);
    if !std::fs::exists(archive_path.as_path()).expect("failed to check archive existence") {
        let url = format!("{RELEASE_BASE_URL}/{asset_name}");
        tracing::info!(
            "downloading prover service archive from {url} to {}",
            archive_path.display()
        );
        let resp = download_prover_binary(&url)
            .await
            .expect("failed to download");
        let body = resp
            .bytes()
            .await
            .expect("failed to read response body")
            .to_vec();
        std::fs::write(archive_path.as_path(), body).expect("failed to write archive");
    }

    let extract_dir = dir.join(format!("{local_binary_name}-extract"));
    if std::fs::exists(extract_dir.as_path()).expect("failed to check extraction dir existence") {
        std::fs::remove_dir_all(extract_dir.as_path())
            .expect("failed to clear previous extraction dir");
    }
    std::fs::create_dir_all(extract_dir.as_path()).expect("failed to create extraction dir");
    let (archive_path_clone, extract_dir_clone) = (archive_path.clone(), extract_dir.clone());
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&archive_path_clone)
            .expect("prover archive exists and is readable");
        tar::Archive::new(flate2::read::GzDecoder::new(file))
            .unpack(&extract_dir_clone)
            .unwrap_or_else(|e| {
                panic!(
                    "failed to unpack prover archive {}: {e}",
                    archive_path_clone.display()
                )
            });
    })
    .await
    .expect("extraction task did not panic");

    let extracted_binary_path =
        find_first_prover_binary(extract_dir.as_path()).unwrap_or_else(|| {
            panic!(
                "failed to locate prover binary after unpacking archive {}",
                archive_path.display()
            )
        });
    std::fs::copy(extracted_binary_path.as_path(), binary_path.as_path())
        .expect("failed to copy extracted prover binary");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut perms = std::fs::metadata(binary_path.as_path())
            .expect("failed to load binary metadata")
            .permissions();
        perms.set_mode(0o755); // Sets rwxr-xr-x
        std::fs::set_permissions(binary_path.as_path(), perms)
            .expect("failed to set binary permissions");
    }
    #[cfg(not(unix))]
    {
        panic!("unsupported platform (UNIX required)");
    }

    binary_path.display().to_string()
}

#[cfg(feature = "prover-tests")]
fn find_first_prover_binary(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_first_prover_binary(path.as_path()) {
                return Some(found);
            }
            continue;
        }

        let Some(file_name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        if file_name.starts_with("zksync-os-prover-service") && !file_name.ends_with(".tar.gz") {
            return Some(path);
        }
    }
    None
}

#[cfg(feature = "prover-tests")]
async fn download_prover_binary(url: &str) -> anyhow::Result<reqwest::Response> {
    use reqwest::{
        Client, StatusCode,
        header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT},
    };

    const DOWNLOAD_MAX_ATTEMPTS: usize = 5;
    const DOWNLOAD_TIMEOUT_SECS: u64 = 60;
    const DOWNLOAD_BASE_BACKOFF_MS: u64 = 500;

    fn is_retryable_status(status: StatusCode) -> bool {
        status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("zksync-os-integration-tests/1.0"),
    );

    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        let bearer = format!("Bearer {}", token.trim());
        match HeaderValue::from_str(&bearer) {
            Ok(value) => {
                headers.insert(AUTHORIZATION, value);
            }
            Err(err) => {
                tracing::warn!("Ignoring invalid GITHUB_TOKEN format: {err}");
            }
        }
    }

    let client = Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .build()?;

    for attempt in 1..=DOWNLOAD_MAX_ATTEMPTS {
        let response = client.get(url).send().await;
        match response {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Ok(response);
                }

                if is_retryable_status(status) && attempt < DOWNLOAD_MAX_ATTEMPTS {
                    let delay_ms = DOWNLOAD_BASE_BACKOFF_MS * attempt as u64;
                    tracing::warn!(
                        "download attempt {attempt}/{DOWNLOAD_MAX_ATTEMPTS} failed with status {status} for {url}; retrying in {delay_ms}ms"
                    );
                    std::thread::sleep(Duration::from_millis(delay_ms));
                    continue;
                }

                anyhow::bail!("download failed with status {status} for {url}");
            }
            Err(err) => {
                if attempt < DOWNLOAD_MAX_ATTEMPTS {
                    let delay_ms = DOWNLOAD_BASE_BACKOFF_MS * attempt as u64;
                    tracing::warn!(
                        "download attempt {attempt}/{DOWNLOAD_MAX_ATTEMPTS} failed for {url}: {err}; retrying in {delay_ms}ms"
                    );
                    std::thread::sleep(Duration::from_millis(delay_ms));
                    continue;
                }

                anyhow::bail!("download request failed for {url}: {err}");
            }
        }
    }
    unreachable!("loop always returns on success or final attempt");
}
