use crate::config::ChainLayout;
use crate::node_log::NodeLogState;
use crate::prover_tester::ProverTester;
use crate::provider::ZksyncTestingProvider;
use crate::rpc_recorder::{HttpRpcRecorder, RpcRecordConfig};
use crate::test_config::{build_node_config, disable_prover_input_generation};
use crate::utils::LockedPort;
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::utils::Eip1559Estimator;
use alloy::providers::{
    DynProvider, PendingTransactionBuilder, Provider, ProviderBuilder, WalletProvider,
};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::{LocalSigner, PrivateKeySigner};
use anyhow::Context;
use backon::ConstantBuilder;
use backon::Retryable;
use reth_tasks::{PanickedTaskError, Runtime, RuntimeBuilder, RuntimeConfig, TokioConfig};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tempfile::TempDir;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tracing::Instrument;
use zksync_os_alloy_ext::network::Zksync;
use zksync_os_alloy_ext::provider::ZksyncApi;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_network::NodeRecord;
use zksync_os_provider::NodeProvider;
use zksync_os_server::ServerPorts;
use zksync_os_server::config::Config;
pub use zksync_os_server::config::{DeploymentFilterConfig, PolicyServiceConfig};
#[cfg(feature = "prover-tests")]
use zksync_os_server::default_protocol_version::PROTOCOL_VERSION_V31_0;
use zksync_os_server::default_protocol_version::{NEXT_PROTOCOL_VERSION, PROTOCOL_VERSION};
use zksync_os_state_full_diffs::FullDiffsState;
use zksync_os_status_server::StatusResponse;
use zksync_os_types::{
    L1PriorityTxType, L1TxType, NodeRole, REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
};

pub mod assert_traits;
pub mod config;
pub mod contracts;
pub mod l1_helpers;
pub mod l1_proxy;
pub mod multi_node;
mod node_log;
mod prover_tester;
pub mod provider;
pub mod rpc_recorder;
pub mod settlement;
pub mod test_config;
pub mod upgrade;
mod utils;
pub mod wallets;

/// L1 chain id as expected by contracts deployed in `l1-state.json.gz`
const L1_CHAIN_ID: u64 = 31337;

pub use zksync_os_integration_tests_macros::test_multisetup;

#[derive(Debug, Clone, Copy)]
pub struct TestCase {
    pub protocol_version: &'static str,
}

impl TestCase {
    pub const fn current_to_l1() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
        }
    }

    pub const fn next_to_l1() -> Self {
        Self {
            protocol_version: NEXT_PROTOCOL_VERSION,
        }
    }

    pub async fn environment(self) -> anyhow::Result<TestEnvironment> {
        TestEnvironment::from_case(self).await
    }
}

pub const CURRENT_TO_L1: TestCase = TestCase::current_to_l1();
pub const NEXT_TO_L1: TestCase = TestCase::next_to_l1();

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
    prepared_runtime: PreparedRuntime,
}

struct PreparedRuntime {
    tempdir: Arc<TempDir>,
}

impl PreparedRuntime {
    async fn new() -> anyhow::Result<Self> {
        Ok(Self {
            tempdir: Arc::new(tempfile::tempdir()?),
        })
    }
}

impl TestEnvironment {
    async fn from_case(case: TestCase) -> anyhow::Result<Self> {
        let chain_layout = ChainLayout::Default {
            protocol_version: case.protocol_version,
        };
        let l1 = AnvilL1::start(chain_layout).await?;
        let prepared_runtime = PreparedRuntime::new().await?;
        Ok(Self {
            l1,
            chain_layout,
            prepared_runtime,
        })
    }

    pub async fn default_config(&self) -> anyhow::Result<Config> {
        let mut config = build_node_config(&self.l1, self.chain_layout, false).await?;
        Tester::bind_runtime_config(
            &self.l1,
            self.prepared_runtime.tempdir.as_ref(),
            &mut config,
        );
        Ok(config)
    }

    pub async fn launch_default(self) -> anyhow::Result<Tester> {
        let config = self.default_config().await?;
        self.launch(config).await
    }

    pub async fn launch(self, mut config: Config) -> anyhow::Result<Tester> {
        if !prover_input_generation_enabled() {
            disable_prover_input_generation(&mut config);
        }
        Tester::bind_runtime_config(
            &self.l1,
            self.prepared_runtime.tempdir.as_ref(),
            &mut config,
        );
        #[cfg(feature = "prover-tests")]
        let enable_prover = !config.prover_api_config.fake_fri_provers.enabled;
        let tester = Tester::launch_node_inner(
            self.l1,
            config,
            self.prepared_runtime.tempdir,
            self.chain_layout,
            None,
            true,
        )
        .await?;
        #[cfg(feature = "prover-tests")]
        if enable_prover {
            let mut sequencer_urls = vec![tester.prover_api_address.clone()];
            for node in &tester.owned_supporting_nodes {
                sequencer_urls.push(
                    node.bound_ports
                        .prover_api
                        .map(|p| format!("http://localhost:{}", p))
                        .expect("supporting node must have prover API port bound for prover tests"),
                );
            }
            spawn_prover_service(&tester, &sequencer_urls, sequencer_urls.len()).await;
        }
        Ok(tester)
    }
}

/// A running primary test node together with its effective config, clients and any supporting
/// runtimes that need to stay alive for the test topology.
#[derive(Debug)]
pub struct Tester {
    l1: AnvilL1,
    pub l2_provider: NodeProvider,
    /// ZKsync OS-specific provider. Generally prefer to use `l2_provider` as we strive for the
    /// system to be Ethereum-compatible. But this can be useful if you need to assert custom fields
    /// that are only present in ZKsync OS response types (`l2ToL1Logs`, `commitTx`, etc).
    pub l2_zk_provider: DynProvider<Zksync>,
    pub l2_wallet: EthereumWallet,

    pub prover_tester: ProverTester,

    runtime: Runtime,
    task_manager_handle: Option<JoinHandle<Result<(), PanickedTaskError>>>,
    config: Config,
    bound_ports: ServerPorts,

    #[allow(dead_code)]
    tempdir: Arc<tempfile::TempDir>,

    // Needed to be able to connect external nodes
    node_record: NodeRecord,
    l2_rpc_address: String,
    status_server_url: String,
    log_state: NodeLogState,
    chain_layout: ChainLayout<'static>,
    owned_supporting_nodes: Vec<SupportingNode>,
    #[cfg(feature = "prover-tests")]
    prover_api_address: String,
}

/// A stopped test node that keeps its database, effective config and L1 alive so it can be
/// started again.
#[derive(Debug)]
pub struct StoppedTester {
    pub(crate) l1: AnvilL1,
    pub(crate) config: Config,
    previous_bound_ports: ServerPorts,
    tempdir: Arc<tempfile::TempDir>,
    log_state: NodeLogState,
    pub(crate) chain_layout: ChainLayout<'static>,
    owned_supporting_nodes: Vec<SupportingNode>,
}

/// The durable parts of a stopped node, cloned via [`StoppedTester::backup`]
/// before a start attempt that a test *expects* the node to refuse: the node's
/// startup guards panic, and the launch consumes the `StoppedTester` — but the
/// database directory, the L1 handle and the config all survive the refusal,
/// so [`Self::restore`] puts a working `StoppedTester` back together.
///
/// The previously bound server ports are carried through so a successful start
/// after the refusal pins the same HTTP ports a plain restart would. Supporting
/// nodes owned by the failed launch are shut down by its drop and are not
/// restored — expected-refusal starts are for plain clusters, which own none.
#[derive(Debug)]
pub struct StoppedTesterBackup {
    l1: AnvilL1,
    config: Config,
    previous_bound_ports: ServerPorts,
    tempdir: Arc<tempfile::TempDir>,
    log_state: NodeLogState,
    chain_layout: ChainLayout<'static>,
}

impl StoppedTesterBackup {
    pub async fn restore(self) -> anyhow::Result<StoppedTester> {
        Ok(StoppedTester {
            l1: self.l1,
            config: self.config,
            previous_bound_ports: self.previous_bound_ports,
            tempdir: self.tempdir,
            log_state: self.log_state,
            chain_layout: self.chain_layout,
            owned_supporting_nodes: Vec::new(),
        })
    }
}

#[derive(Debug)]
pub struct SupportingNode {
    runtime: Runtime,
    pub prover_tester: ProverTester,
    #[cfg(feature = "prover-tests")]
    bound_ports: ServerPorts,
    _tempdir: Arc<TempDir>,
}

impl Tester {
    pub fn config(&self) -> &Config {
        &self.config
    }

    fn apply_external_node_defaults(&self, config: &mut Config) {
        config.general_config.node_role = NodeRole::ExternalNode;
        // External nodes are never committee members: they follow the finalized chain
        // over the replay protocol regardless of how the upstream node sequences it —
        // so a validator's consensus settings (keys, committee, enablement) must not
        // travel into an EN cloned from its config.
        config.consensus_config = Default::default();
        config.network_config.boot_nodes = vec![self.node_record.into()];
        // The clone carries the upstream node's network identity; an EN needs
        // its own (a pre-set secret is preserved by `bind_runtime_config` as a
        // deliberate identity — here it would collide with the upstream's port).
        // Clearing the key makes `bind_runtime_config` assign a fresh identity
        // and an ephemeral port.
        config.network_config.secret_key = None;
        config.general_config.main_node_rpc_url = Some(self.l2_rpc_address.clone());
        config.prover_api_config.fake_fri_provers.enabled = true;
        config.prover_api_config.fake_snark_provers.enabled = true;
        config.prover_input_generator_config.logging_enabled = false;
        config.batch_verification_config.server_enabled = false;
        config.l1_sender_config.pubdata_mode = None;
    }

    pub fn l1_provider(&self) -> &NodeProvider {
        &self.l1.provider
    }

    pub fn l1_wallet(&self) -> &EthereumWallet {
        &self.l1.wallet
    }

    /// Returns true if the node's runtime has reported a critical-task panic.
    ///
    /// Mirrors what a production orchestrator observes when a `reth_tasks` critical task
    /// panics: the runtime is dying and the node should be respawned. Non-destructive — the
    /// task-manager handle is left in place so the caller can still consume it via
    /// [`Self::wait_for_fatal_error_with_timeout`] if desired.
    pub fn has_crashed(&self) -> bool {
        self.task_manager_handle
            .as_ref()
            .is_some_and(|h| h.is_finished())
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

    pub async fn status(&self) -> anyhow::Result<StatusResponse> {
        let response = reqwest::get(format!("{}/status", self.status_server_url))
            .await?
            .error_for_status()?;
        Ok(response.json::<StatusResponse>().await?)
    }

    /// The consensus runtime's own prometheus registry, as served for scraping.
    pub async fn consensus_metrics(&self) -> anyhow::Result<String> {
        let response = reqwest::get(format!(
            "{}/status/consensus-metrics",
            self.status_server_url
        ))
        .await?
        .error_for_status()?;
        Ok(response.text().await?)
    }

    /// Owned handles for driving a deposit off-task (see [`deposit_l1_to_l2`]):
    /// the deposit helper blocks on the L2 receipt, and scenarios that park the
    /// chain mid-deposit need the call running in the background.
    pub fn deposit_handles(&self) -> (AnvilL1, NodeProvider, DynProvider<Zksync>) {
        (
            self.l1.clone(),
            self.l2_provider.clone(),
            self.l2_zk_provider.clone(),
        )
    }

    /// One L1→L2 deposit through this node (see [`deposit_l1_to_l2`]).
    pub async fn deposit(&self, beneficiary: Address, amount: U256) -> anyhow::Result<B256> {
        deposit_l1_to_l2(
            &self.l1,
            &self.l2_provider,
            &self.l2_zk_provider,
            beneficiary,
            amount,
        )
        .await
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

    /// Gracefully shut down the node while keeping its database and L1 alive for a later restart.
    ///
    /// Returns a new `Tester` connected to the restarted node. The original `Tester` is consumed.
    ///
    /// A later restart preserves HTTP ports. Port-0 p2p networking may get a new OS-assigned port.
    pub async fn stop(self) -> anyhow::Result<StoppedTester> {
        let Self {
            runtime,
            l1,
            config,
            bound_ports,
            tempdir,
            log_state,
            chain_layout,
            owned_supporting_nodes,
            ..
        } = self;
        // NOTE: supporting nodes are kept alive across stop/start; they are only torn down in
        // `StoppedTester::shutdown()` or when `StoppedTester` is dropped.
        shutdown_runtime(runtime).await?;
        // The consensus stack outlives the node runtime by design; a stopped
        // tester must mean the whole instance is gone (restarts reopen its
        // storage, and a test ending here must not race live threads at exit).
        wait_for_consensus_storage_released(&config).await?;
        Ok(StoppedTester {
            l1,
            tempdir,
            log_state,
            chain_layout,
            config,
            previous_bound_ports: bound_ports,
            owned_supporting_nodes,
        })
    }

    /// Restart keeps the same config by default.
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
            config,
            // Keep the storage directory alive until consensus has wound down:
            // deleting it under a live instance turns its teardown into a crash.
            tempdir,
            ..
        } = self;
        drop(owned_supporting_nodes);
        shutdown_runtime(runtime).await?;
        // A test returning from shutdown may end the process; consensus threads
        // still winding down at that point die mid-teardown (observed as
        // SIGABRT at exit under load).
        wait_for_consensus_storage_released(&config).await?;
        // The storage lock covers every task the consensus runtime can track,
        // but a task that never subscribed to the stop signal can hold the
        // runtime itself alive a beat longer — and a process exit under its
        // worker threads aborts in pthread teardown. A short grace absorbs
        // that last gasp.
        if config.consensus_config.enabled {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        drop(tempdir);
        Ok(())
    }

    pub(crate) async fn launch_with_new_runtime(
        l1: AnvilL1,
        chain_layout: ChainLayout<'static>,
        mut config: Config,
    ) -> anyhow::Result<Self> {
        let tempdir = Arc::new(tempfile::tempdir()?);
        Self::bind_runtime_config(&l1, tempdir.as_ref(), &mut config);
        Self::launch_node_inner(l1, config, tempdir, chain_layout, None, true).await
    }

    /// Like [`Self::launch_with_new_runtime`], but the node starts on a *copy* of
    /// another node's chain databases — the snapshot-distribution step of a
    /// migration, where every new validator receives the drained sequencer's chain
    /// state. `seed_rocks_from` is the source node's RocksDB root.
    pub(crate) async fn launch_with_seeded_state(
        l1: AnvilL1,
        chain_layout: ChainLayout<'static>,
        mut config: Config,
        seed_rocks_from: &std::path::Path,
    ) -> anyhow::Result<Self> {
        let tempdir = Arc::new(tempfile::tempdir()?);
        Self::bind_runtime_config(&l1, tempdir.as_ref(), &mut config);
        copy_dir_recursively(seed_rocks_from, &config.general_config.rocks_db_path)?;
        Self::launch_node_inner(l1, config, tempdir, chain_layout, None, true).await
    }

    fn bind_runtime_config(l1: &AnvilL1, tempdir: &TempDir, config: &mut Config) {
        config.general_config.rocks_db_path = tempdir.path().join("rocksdb");
        config.l1_provider_config.rpc_url = l1.address.clone();
        config.rpc_config.address = "0.0.0.0:0".to_string();
        config.prover_api_config.address = "0.0.0.0:0".to_string();
        config.prover_api_config.proof_storage.path = tempdir.path().join("proof_storage_path");
        config.status_server_config.address = "0.0.0.0:0".to_string();
        config.network_config.address = Ipv4Addr::LOCALHOST;
        config.network_config.interface = None;
        // A pre-set secret key marks a deliberate network identity: peers hold
        // boot-node records derived from (key, port) — committee meshes,
        // restarts — so both must survive rebinding. Everything else gets a
        // throwaway key and asks the server for a fresh ephemeral port (the
        // server turns port 0 into a concrete TCP+UDP p2p port immediately
        // before building reth's network config, so the advertised ENR remains
        // dialable without test-side probing). The port alone can't be the
        // signal: its config default is non-zero.
        if config.network_config.secret_key.is_none() {
            config.network_config.port = 0;
            config.network_config.secret_key = Some(zksync_os_network::rng_secret_key());
        }
        // local_dev.yaml arms the dev-mode revert-on-divergence, which config
        // validation rejects under consensus (a finalized block cannot be locally
        // rebuilt). In-process test nodes skip config validation, but their configs
        // should stay ones the real binary would accept.
        if config.consensus_config.enabled {
            config
                .sequencer_config
                .revm_consistency_checker_revert_on_divergence = false;
        }
    }

    async fn launch_node_inner(
        l1: AnvilL1,
        mut config: Config,
        tempdir: Arc<TempDir>,
        chain_layout: ChainLayout<'static>,
        log_state: Option<NodeLogState>,
        wait_for_initial_deposit: bool,
    ) -> anyhow::Result<Self> {
        // In-process fake provers use job managers directly; keep the HTTP API only for tests
        // that can hand jobs to external prover workers.
        if config.prover_api_config.fake_fri_provers.enabled
            && config.prover_api_config.fake_snark_provers.enabled
        {
            config.prover_api_config.enabled = false;
        }

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

        let runtime = RuntimeBuilder::new(
            RuntimeConfig::default().with_tokio(TokioConfig::existing_handle(Handle::current())),
        )
        .build()
        .expect("failed to build runtime");
        let node_span = tracing::info_span!(
            "node",
            node = %log_tag,
            role = %node_role,
        );
        tracing::info!(parent: &node_span, "Launching test node");
        // Node startup runs inline on the test task, and the node's style for a
        // fatal startup condition is a panic (e.g. the L1 mutual-exclusion guard
        // refusing to start a second settler). In production that is a loud
        // process exit; in-process it would unwind the *test*. Catch it and
        // surface a launch error instead, so tests can assert on failed launches
        // the same way they assert on any other `Err`.
        //
        // One panic class is transient and retried instead of surfaced: a
        // restarted node can find its own databases still locked, because the
        // previous incarnation's RocksDB handles drop on the process-shared
        // blocking pool and can lag its runtime shutdown. The conflict clears
        // as soon as the straggling drop lands.
        let launch_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let bound_ports = loop {
            let launch = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
                zksync_os_server::run::<FullDiffsState>(&runtime, config.clone())
                    .instrument(node_span.clone()),
            ))
            .await;
            let panic = match launch {
                Ok(bound_ports) => break bound_ports,
                Err(panic) => panic,
            };
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&'static str>().copied())
                .unwrap_or("<non-string panic payload>");
            let stale_db_lock = message.contains("lock hold by current process");
            if stale_db_lock && tokio::time::Instant::now() < launch_deadline {
                tracing::warn!(
                    parent: &node_span,
                    "previous incarnation's database handles are still closing; retrying launch"
                );
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            anyhow::bail!("node startup panicked: {message}");
        };
        let task_manager_handle = runtime
            .take_task_manager_handle()
            .expect("Runtime must contain a TaskManager handle");

        let l2_rpc_ws_url = format!("ws://localhost:{}", bound_ports.rpc);
        let l2_rpc_address = format!("http://localhost:{}", bound_ports.rpc);
        let status_server_url = bound_ports
            .status
            .map(|p| format!("http://localhost:{}", p))
            .unwrap_or_default();
        let network_secret_key = config
            .network_config
            .secret_key
            .as_ref()
            .context("network secret key should be present in test config")?;
        let mut node_record = NodeRecord::from_secret_key(
            SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                bound_ports.network.map(|p| p.tcp).unwrap_or(0),
            ),
            network_secret_key,
        );
        if let Some(network_ports) = bound_ports.network {
            node_record.udp_port = network_ports.udp;
        }
        #[cfg(feature = "prover-tests")]
        let prover_api_address = bound_ports
            .prover_api
            .map(|p| format!("http://localhost:{}", p))
            .unwrap_or_default();

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

        let prover_tester = ProverTester::new(
            NodeProvider::new(l1.provider.clone()).await?,
            NodeProvider::new(l2_provider.clone()).await?,
            DynProvider::new(l2_zk_provider.clone()),
        );
        let tester = Tester {
            l1,
            l2_provider: NodeProvider::new(l2_provider.clone()).await?,
            l2_zk_provider: DynProvider::new(l2_zk_provider.clone()),
            l2_wallet,
            prover_tester,
            runtime,
            task_manager_handle: Some(task_manager_handle),
            config,
            bound_ports,
            l2_rpc_address,
            status_server_url,
            node_record,
            log_state,
            tempdir: tempdir.clone(),
            chain_layout,
            owned_supporting_nodes: Vec::new(),
            #[cfg(feature = "prover-tests")]
            prover_api_address,
        };
        if wait_for_initial_deposit {
            tester.wait_for_initial_deposit().await?;
        }
        Ok(tester)
    }

    pub fn owned_supporting_nodes(&self) -> &[SupportingNode] {
        &self.owned_supporting_nodes
    }
}

impl StoppedTester {
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Clones the parts a refused start leaves recoverable (see
    /// [`StoppedTesterBackup`]).
    pub fn backup(&self) -> StoppedTesterBackup {
        StoppedTesterBackup {
            l1: self.l1.clone(),
            config: self.config.clone(),
            previous_bound_ports: self.previous_bound_ports,
            tempdir: self.tempdir.clone(),
            log_state: self.log_state.clone(),
            chain_layout: self.chain_layout,
        }
    }

    pub fn l1_provider(&self) -> &NodeProvider {
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
            previous_bound_ports,
            config: _,
            ..
        } = self;
        let mut config = config;
        preserve_http_ports_on_restart(&mut config, previous_bound_ports)?;
        // A committee validator restarts on its configured concrete p2p port
        // (its peers hold boot records derived from it); wait for the previous
        // incarnation's listener to be fully released before rebinding.
        // Ephemeral-port nodes (port 0) get a fresh port and skip this.
        if config.network_config.port != 0 {
            wait_for_port_to_be_unused(config.network_config.port).await?;
        }
        wait_for_rocksdb_locks_released(&config.general_config.rocks_db_path).await?;
        // A batcher must not come back up while its previous incarnation's L1
        // transactions are still in flight — a commit landing after the new
        // session's startup snapshot reads as a foreign settler's and trips the
        // unexpected-commit guard (whose designed remedy is another restart).
        if config.batcher_config.enabled {
            wait_for_operator_l1_quiescence(&l1, &config).await?;
        }
        let mut tester = Tester::launch_node_inner(
            l1,
            config,
            tempdir,
            chain_layout,
            Some(log_state.restarted()),
            false,
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

fn preserve_http_ports_on_restart(
    config: &mut Config,
    previous_bound_ports: ServerPorts,
) -> anyhow::Result<()> {
    config.rpc_config.address =
        socket_address_with_port(&config.rpc_config.address, previous_bound_ports.rpc)?;
    if let Some(status_port) = previous_bound_ports.status {
        config.status_server_config.address =
            socket_address_with_port(&config.status_server_config.address, status_port)?;
    }
    if let Some(prover_api_port) = previous_bound_ports.prover_api {
        config.prover_api_config.address =
            socket_address_with_port(&config.prover_api_config.address, prover_api_port)?;
    }
    Ok(())
}

fn socket_address_with_port(address: &str, port: u16) -> anyhow::Result<String> {
    let mut address: SocketAddr = address
        .parse()
        .with_context(|| format!("failed to parse socket address {address:?}"))?;
    address.set_port(port);
    Ok(address.to_string())
}

impl Drop for SupportingNode {
    fn drop(&mut self) {
        let _ = self
            .runtime
            .graceful_shutdown_with_timeout(NODE_SHUTDOWN_TIMEOUT);
    }
}

const PORT_ACQUISITION_TIMEOUT: Duration = Duration::from_secs(30);
const PORT_ACQUISITION_POLL_INTERVAL: Duration = Duration::from_millis(100);

async fn wait_for_port_to_be_unused(port: u16) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + PORT_ACQUISITION_TIMEOUT;
    loop {
        match LockedPort::check_port_is_unused(port).await {
            Ok(_) => return Ok(()),
            Err(err) if tokio::time::Instant::now() < deadline => {
                tracing::info!(port, %err, "waiting for port to become unused");
                tokio::time::sleep(PORT_ACQUISITION_POLL_INTERVAL).await;
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("port {port} did not become unused within {PORT_ACQUISITION_TIMEOUT:?}")
                });
            }
        }
    }
}

/// Recursively copies a directory tree — the snapshot-distribution step of a
/// migration, where a fresh validator starts on a copy of the drained sequencer's
/// chain databases.
fn copy_dir_recursively(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursively(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Waits until every RocksDB instance under `rocks_db_path` has released its `LOCK`
/// file. `graceful_shutdown` returning does not guarantee storage handles are
/// dropped — pipeline teardown can lag a moment — and an in-process relaunch that
/// wins that race dies with "lock hold by current process: .../LOCK". The faster
/// node startup gets, the more often the relaunch wins, so the gate belongs here
/// rather than in sleeps sprinkled over tests. (The multi-node consensus harness
/// gates the same way on the consensus instance lock.)
pub async fn wait_for_rocksdb_locks_released(
    rocks_db_path: &std::path::Path,
) -> anyhow::Result<()> {
    use fs2::FileExt as _;
    let deadline = tokio::time::Instant::now() + PORT_ACQUISITION_TIMEOUT;
    loop {
        let mut held_lock = None;
        // Each database is a direct subdirectory holding its own `LOCK` file.
        if let Ok(entries) = std::fs::read_dir(rocks_db_path) {
            for entry in entries.flatten() {
                let lock_path = entry.path().join("LOCK");
                let Ok(file) = std::fs::File::open(&lock_path) else {
                    continue;
                };
                if file.try_lock_exclusive().is_err() {
                    held_lock = Some(lock_path);
                    break;
                }
                let _ = fs2::FileExt::unlock(&file);
            }
        }
        let Some(held_lock) = held_lock else {
            return Ok(());
        };
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "rocksdb lock at {} is still held {:?} after the node stopped",
            held_lock.display(),
            PORT_ACQUISITION_TIMEOUT,
        );
        tokio::time::sleep(PORT_ACQUISITION_POLL_INTERVAL).await;
    }
}

/// Waits until no consensus instance holds the storage under `config`'s rocksdb
/// path. The node's consensus stack winds down asynchronously after the node
/// runtime stops, and its storage lock is released only once every consensus
/// task is gone — so this returning means the instance is truly dead: safe to
/// restart over the same storage, safe for the test process to exit without
/// racing a live runtime's threads.
pub async fn wait_for_consensus_storage_released(config: &Config) -> anyhow::Result<()> {
    if !config.consensus_config.enabled {
        return Ok(());
    }
    let instance_lock = zksync_os_server::consensus::instance_lock_path(
        &config.general_config.rocks_db_path.join("consensus"),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        // The lock's parent directory may not exist (consensus never ran, or its
        // storage was wiped) — trivially nobody holds it, but the probe needs
        // the directory to create its file.
        if let Some(parent) = instance_lock.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(probe) = std::fs::File::create(&instance_lock)
            && fs2::FileExt::try_lock_exclusive(&probe).is_ok()
        {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "the consensus instance did not release its storage within 120s of the node stopping",
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits until the node's L1 operator addresses have no in-flight L1
/// transactions. A settler restarted while its previous incarnation's commit is
/// still being digested by the L1 node trips its own mutual-exclusion guard:
/// the commit lands after the new session's startup snapshot and reads as a
/// foreign settler's. Real deployments answer that with another restart
/// (startup reconciles from L1); tests must simply not manufacture the
/// situation.
///
/// Nonce parity (pending == latest) alone is not enough: a transaction the L1
/// node has *received* but not yet admitted to its pool is invisible to both
/// counts, and on a loaded host that window is real. So after parity, a probe
/// transaction is pushed through and mined — anything received before the
/// probe lands with or before it — and parity is required to survive the
/// probe round-trip.
async fn wait_for_operator_l1_quiescence(l1: &AnvilL1, config: &Config) -> anyhow::Result<()> {
    use crate::assert_traits::ReceiptAssert as _;
    use alloy::primitives::Address;
    use alloy::providers::ext::AnvilApi as _;

    let signers = [
        &config.l1_sender_config.operator_commit_sk,
        &config.l1_sender_config.operator_prove_sk,
        &config.l1_sender_config.operator_execute_sk,
    ];
    let mut addresses = Vec::new();
    for signer in signers.into_iter().flatten() {
        addresses.push(signer.address().await?);
    }

    let in_flight = |addresses: Vec<Address>| async move {
        for address in addresses {
            let pending = l1.provider.get_transaction_count(address).pending().await?;
            let latest = l1.provider.get_transaction_count(address).latest().await?;
            if pending != latest {
                return anyhow::Ok(Some((address, pending, latest)));
            }
        }
        anyhow::Ok(None)
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some((address, pending, latest)) = in_flight(addresses.clone()).await? {
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "operator {address} still has L1 transactions in flight \
                 (pending nonce {pending}, latest {latest}) 60s after the node stopped",
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        // The flush barrier: a no-op self-transfer from a throwaway identity,
        // mined before we look again.
        let probe = Address::repeat_byte(0xF1);
        l1.provider
            .anvil_set_balance(probe, U256::from(1_000_000_000_000_000_000u128))
            .await?;
        l1.provider.anvil_impersonate_account(probe).await?;
        let hash = l1
            .provider
            .anvil_send_impersonated_transaction(
                TransactionRequest::default().from(probe).to(probe),
            )
            .await?;
        PendingTransactionBuilder::new(l1.provider.root().clone(), hash)
            .expect_successful_receipt()
            .await?;

        if in_flight(addresses.clone()).await?.is_none() {
            return Ok(());
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
    l2_provider: &NodeProvider,
    l2_zk_provider: &DynProvider<Zksync>,
    l2_wallet: &EthereumWallet,
) -> anyhow::Result<()> {
    // One funding at a time per test process: concurrently starting testers (a
    // multi-node cluster) share the L2 wallet *and* the L1 rich wallet, so
    // parallel deposits race nonces and fee estimates — the loser's deposit
    // reverts and startup dies. The first holder funds; the rest see the
    // balance and return.
    static FUNDING_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _funding = FUNDING_LOCK.lock().await;

    let beneficiary = l2_wallet.default_signer().address();
    let balance = l2_provider.get_balance(beneficiary).await?;
    if balance > U256::ZERO {
        return Ok(());
    }

    let amount = U256::from(1_000_000_000_000_000_000u128) * U256::from(1_000u64);
    deposit_l1_to_l2(l1, l2_provider, l2_zk_provider, beneficiary, amount).await?;

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

/// One L1→L2 deposit — a priority transaction — of `amount` to `beneficiary`,
/// waited through to its L2 receipt. Returns the L2 transaction hash the
/// priority op is included under.
pub async fn deposit_l1_to_l2(
    l1: &AnvilL1,
    l2_provider: &NodeProvider,
    l2_zk_provider: &DynProvider<Zksync>,
    beneficiary: Address,
    amount: U256,
) -> anyhow::Result<B256> {
    use crate::assert_traits::ReceiptAssert as _;
    let chain_id = l2_provider.get_chain_id().await?;
    let bridgehub = Bridgehub::new(
        l2_zk_provider.get_bridgehub_contract().await?,
        l1.provider.clone(),
        chain_id,
    );
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
        // A reverted deposit must name itself — a bare `get_receipt` would
        // surface as "no L1->L2 logs" below, hiding the actual failure.
        .expect_successful_receipt()
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

    Ok(l2_tx_hash)
}

fn prover_input_generation_enabled() -> bool {
    std::env::var("NEXTEST_PROFILE").as_deref() != Ok("no-pig")
}

#[derive(Debug, Clone)]
pub struct AnvilL1 {
    pub address: String,
    pub provider: NodeProvider,
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

        // --slots-in-an-epoch defines what blocks are "finalized" in Anvil, last finalized block is `latest - 2 * slots_in_an_epoch`
        // so we set block time to 0.25s and slots in epoch set to 10 and finalization delays is about 10*0.25s*2=5s which is reasonable for tests.
        let provider = ProviderBuilder::new().connect_anvil_with_wallet_and_config(|anvil| {
            anvil
                .chain_id(L1_CHAIN_ID)
                .arg("--block-time")
                .arg("0.25")
                .arg("--mixed-mining")
                .arg("--load-state")
                .arg(l1_state_path)
                .arg("--slots-in-an-epoch")
                .arg("10")
        })?;
        let address = provider.inner().anvil().endpoint();

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

        // `NodeProvider::new` probes Anvil's capabilities over a transport with no request
        // timeout; an Anvil that wedges right after passing the readiness check above would
        // otherwise hang the test until nextest's terminate timeout.
        let provider = (|| async {
            tokio::time::timeout(Duration::from_secs(10), NodeProvider::new(provider.clone()))
                .await
                .context("timed out probing L1 node capabilities")?
                .context("failed to probe L1 node capabilities")
        })
        .retry(
            ConstantBuilder::default()
                .with_delay(Duration::from_millis(200))
                .with_max_times(5),
        )
        .notify(|err: &anyhow::Error, dur: Duration| {
            tracing::info!(%err, ?dur, "retrying L1 node capability probing");
        })
        .await?;

        Ok(Self {
            address,
            provider,
            wallet,
            _tempdir: Arc::new(tempdir),
        })
    }
}

#[cfg(feature = "prover-tests")]
async fn spawn_prover_service(tester: &Tester, sequencer_urls: &[String], iterations: usize) {
    let protocol_version = tester.chain_layout.protocol_version();
    let app_bin_path = match protocol_version {
        PROTOCOL_VERSION => utils::materialize_multiblock_batch_bin(
            &tester.tempdir.path().join("app_bins"),
            "v6",
            zksync_os_multivm::apps::v6::MULTIBLOCK_BATCH,
        ),
        PROTOCOL_VERSION_V31_0 => utils::materialize_multiblock_batch_bin(
            &tester.tempdir.path().join("app_bins"),
            "v7",
            zksync_os_multivm::apps::v7::MULTIBLOCK_BATCH,
        ),
        _ => panic!("unsupported protocol version for prover tests"),
    };
    let trusted_setup_file = std::env::var("COMPACT_CRS_FILE").unwrap();
    let output_dir = tester.tempdir.path().join("outputs");
    std::fs::create_dir_all(&output_dir).unwrap();

    let path =
        download_prover_and_unpack(protocol_version, cfg!(feature = "gpu-prover-tests")).await;

    let mut child = tokio::process::Command::new(path)
        .arg("--sequencer-urls")
        .arg(sequencer_urls.join(","))
        .arg("--app-bin-path")
        .arg(app_bin_path)
        .arg("--circuit-limit")
        .arg("10000")
        .arg("--output-dir")
        .arg(output_dir)
        .arg("--trusted-setup-file")
        .arg(trusted_setup_file)
        .arg("--iterations")
        .arg(iterations.to_string())
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

#[cfg(feature = "prover-tests")]
fn prover_release_for_protocol(protocol_version: &str) -> &'static str {
    match protocol_version {
        PROTOCOL_VERSION => "v0.7.1",
        PROTOCOL_VERSION_V31_0 => "v0.8.0",
        _ => {
            panic!("unsupported protocol version `{protocol_version}` for prover binary selection")
        }
    }
}

#[cfg(feature = "prover-tests")]
async fn download_prover_and_unpack(protocol_version: &str, gpu: bool) -> String {
    let release_version = prover_release_for_protocol(protocol_version);
    let release_base_url = format!(
        "https://github.com/matter-labs/zksync-airbender-prover/releases/download/{release_version}"
    );

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let asset_name = match (os, arch, gpu) {
        ("linux", "x86_64", true) => {
            format!(
                "zksync-os-prover-service-{release_version}-x86_64-unknown-linux-gnu-gpu.tar.gz"
            )
        }
        ("linux", "x86_64", false) => {
            format!(
                "zksync-os-prover-service-{release_version}-x86_64-unknown-linux-gnu-cpu.tar.gz"
            )
        }
        ("macos", _, true) => {
            panic!("GPU prover binary is not available for macOS in {release_version}")
        }
        ("macos", _, false) => {
            format!("zksync-os-prover-service-{release_version}-universal-apple-darwin-cpu.tar.gz")
        }
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

    let archive_path = dir.join(&asset_name);
    if !std::fs::exists(archive_path.as_path()).expect("failed to check archive existence") {
        let url = format!("{release_base_url}/{asset_name}");
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
