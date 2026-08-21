use crate::config::ChainLayout;
use crate::node_log::NodeLogState;
use crate::prover_tester::ProverTester;
use crate::provider::ZksyncTestingProvider;
use crate::rpc_recorder::{HttpRpcRecorder, RpcRecordConfig};
use crate::test_config::{build_node_config, disable_prover_input_generation};
use alloy::network::{EthereumWallet, TransactionBuilder};
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
use zksync_os_server::default_protocol_version::PROTOCOL_VERSION;
#[cfg(feature = "prover-tests")]
use zksync_os_server::default_protocol_version::{PROTOCOL_VERSION_V30_2, PROTOCOL_VERSION_V31_0};
use zksync_os_state_full_diffs::FullDiffsState;
use zksync_os_status_server::StatusResponse;
use zksync_os_types::{
    L1PriorityTxType, L1TxType, NodeRole, REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
};

pub mod assert_traits;
pub mod config;
pub mod contracts;
pub mod l1_helpers;
mod leash;
mod node_log;
mod prover_tester;
pub mod provider;
pub mod rpc_recorder;
pub mod test_config;
pub mod upgrade;
#[cfg(feature = "prover-tests")]
mod utils;
pub mod wallets;

/// L1 chain id as expected by contracts deployed in `l1-state.json.gz`
const L1_CHAIN_ID: u64 = 31337;

pub use zksync_os_integration_tests_macros::test_multisetup;

#[derive(Debug, Clone, Copy)]
pub struct TestCase {
    chain_layout: ChainLayout<'static>,
}

impl TestCase {
    /// A default-layout chain pinned to an explicit protocol version, for
    /// tests that drive a specific version rather than the current default.
    pub const fn at_protocol_version(protocol_version: &'static str) -> Self {
        Self {
            chain_layout: ChainLayout::Default { protocol_version },
        }
    }

    pub const fn current_to_l1() -> Self {
        Self {
            chain_layout: ChainLayout::Default {
                protocol_version: PROTOCOL_VERSION,
            },
        }
    }

    /// The current protocol version on the multiprover L1. Its verifier accepts
    /// a combined Airbender + ZiSK proof only, so a real settlement there proves
    /// both lanes agreed. Fake proofs settle as they do on the default chain.
    pub const fn current_to_multiprover_l1() -> Self {
        Self {
            chain_layout: ChainLayout::Multiprover {
                protocol_version: PROTOCOL_VERSION,
            },
        }
    }

    pub fn protocol_version(self) -> &'static str {
        self.chain_layout.protocol_version()
    }

    pub fn chain_layout(self) -> ChainLayout<'static> {
        self.chain_layout
    }

    pub async fn environment(self) -> anyhow::Result<TestEnvironment> {
        TestEnvironment::from_case(self).await
    }
}

// A next-version lane (fresh chain at the next protocol version) needs local-chain fixtures for
// v32.0; reintroduce it once they are generated. Until then v32 is covered via in-test upgrades.
pub const CURRENT_TO_L1: TestCase = TestCase::current_to_l1();
pub const CURRENT_TO_MULTIPROVER_L1: TestCase = TestCase::current_to_multiprover_l1();

/// Fresh chain at v30.2 — the oldest protocol version with live proving support (V6).
#[cfg(feature = "prover-tests")]
pub const V30_TO_L1: TestCase = TestCase::at_protocol_version(PROTOCOL_VERSION_V30_2);

/// Set of private keys for batch verification participants.
pub const BATCH_VERIFICATION_KEYS: [&str; 2] = [
    "0x7094f4b57ed88624583f68d2f241858f7dafb6d2558bc22d18991690d36b4e47",
    "0xf9306dd03807c08b646d47c739bd51e4d2a25b02bad0efb3d93f095982ac98cd",
];
/// Shutdown completes in <5 seconds when there is no CPU starvation. But because prover input
/// generator runs its CPU-bound task on a blocking thread it can significantly slow down graceful
/// shutdown. Keep 60s until V7 proving support is dropped (V8 generates prover input natively
/// at batch seal, without the blocking RISC-V simulator).
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
        let chain_layout = case.chain_layout();
        let l1 = AnvilL1::start(chain_layout).await?;
        let prepared_runtime = PreparedRuntime::new().await?;
        Ok(Self {
            l1,
            chain_layout,
            prepared_runtime,
        })
    }

    /// Anvil's direct RPC endpoint, e.g. to put a fault-injecting proxy in front of it.
    pub fn l1_rpc_url(&self) -> &str {
        &self.l1.address
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

    pub async fn launch(self, config: Config) -> anyhow::Result<Tester> {
        self.launch_impl(config, true, None).await
    }

    /// Launches the node with its L1 RPC routed through `l1_rpc_url` (e.g. a fault-injecting
    /// proxy in front of anvil) instead of anvil's direct endpoint. Test-side helpers
    /// (`Tester::l1_provider()` etc.) still talk to anvil directly.
    pub async fn launch_with_l1_rpc(
        self,
        config: Config,
        l1_rpc_url: String,
    ) -> anyhow::Result<Tester> {
        self.launch_impl(config, true, Some(l1_rpc_url)).await
    }

    /// Launch without auto-spawning prover services even when the fake
    /// provers are disabled (`prover-tests`). For tests that orchestrate the
    /// real provers manually — the two-lane runs must control when each lane
    /// holds the single GPU.
    pub async fn launch_without_provers(self, config: Config) -> anyhow::Result<Tester> {
        self.launch_impl(config, false, None).await
    }

    async fn launch_impl(
        self,
        mut config: Config,
        auto_spawn_provers: bool,
        l1_rpc_url: Option<String>,
    ) -> anyhow::Result<Tester> {
        #[cfg(not(feature = "prover-tests"))]
        let _ = auto_spawn_provers;
        if !prover_input_generation_enabled() {
            disable_prover_input_generation(&mut config);
        }
        Tester::bind_runtime_config(
            &self.l1,
            self.prepared_runtime.tempdir.as_ref(),
            &mut config,
        );
        // After `bind_runtime_config`, which points the node at anvil directly:
        // a caller asking for a different endpoint (a fault-injecting proxy in
        // front of anvil) must win, or the proxy is silently bypassed.
        if let Some(l1_rpc_url) = l1_rpc_url {
            config.l1_provider_config.rpc_url = l1_rpc_url;
        }
        #[cfg(feature = "prover-tests")]
        let enable_prover =
            auto_spawn_provers && !config.prover_api_config.fake_fri_provers.enabled;
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
    l1: AnvilL1,
    config: Config,
    previous_bound_ports: ServerPorts,
    tempdir: Arc<tempfile::TempDir>,
    log_state: NodeLogState,
    chain_layout: ChainLayout<'static>,
    owned_supporting_nodes: Vec<SupportingNode>,
}

#[derive(Debug)]
pub struct SupportingNode {
    runtime: Runtime,
    pub prover_tester: ProverTester,
    #[cfg(feature = "prover-tests")]
    bound_ports: ServerPorts,
    _tempdir: Arc<TempDir>,
}

/// How far the batcher has sealed, read from the `batcher` component's
/// `processed` coordinates on `/status/pipeline`.
#[derive(Debug, Clone, Copy)]
pub struct BatcherProgress {
    /// The last block that the batcher put into a sealed batch. It is `0`
    /// before the batcher seals its first batch.
    pub last_included_block: u64,
    /// The last batch that the batcher sealed. Batches are numbered `1..N`, so
    /// this is also the number of sealed batches. It is `0` before the batcher
    /// seals its first batch.
    pub last_sealed_batch: u64,
}

impl Tester {
    /// Prover API base URL of this node, if the prover API server is bound.
    /// Stable across [`Tester::stop`] / restart (HTTP ports are preserved).
    pub fn prover_api_url(&self) -> Option<String> {
        self.bound_ports
            .prover_api
            .map(|p| format!("http://localhost:{p}"))
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn apply_external_node_defaults(&self, config: &mut Config) {
        config.general_config.node_role = NodeRole::ExternalNode;
        config.network_config.boot_nodes = vec![self.node_record.into()];
        // This config is cloned from the main node; ask startup to pick a fresh concrete TCP+UDP
        // port and identity so the external node doesn't collide with it.
        config.network_config.port = 0;
        config.network_config.secret_key = Some(zksync_os_network::rng_secret_key());
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

    /// The batcher's *actual sealing progress*: how far the downstream proving
    /// and commit stages have advanced does not change it, and — because the
    /// batcher sits downstream of prover-input generation — it is already past
    /// any input-generation lag.
    pub async fn batcher_progress(&self) -> anyhow::Result<BatcherProgress> {
        let components: serde_json::Value =
            reqwest::get(format!("{}/status/pipeline", self.status_server_url))
                .await?
                .error_for_status()?
                .json()
                .await?;
        let processed = components
            .as_array()
            .and_then(|comps| comps.iter().find(|c| c["name"] == "batcher"))
            .and_then(|batcher| batcher.get("processed"));
        let field = |name: &str| {
            processed
                .and_then(|p| p.get(name))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        Ok(BatcherProgress {
            last_included_block: field("block_number"),
            last_sealed_batch: field("batch_number"),
        })
    }

    /// Drive the chain to exactly `target_batches` sealed batches.
    ///
    /// Waits for the boot batches to settle, then sends one transfer per
    /// remaining batch and gates each send on its batch's seal. Sealing rides
    /// the test config's short batch timeout: `tx_per_batch_limit = 1` is not
    /// usable because the genesis batch is one multi-transaction block.
    /// Requires the status server.
    pub async fn drive_to_exact_sealed_batches(&self, target_batches: u64) -> anyhow::Result<()> {
        const SETTLE_TIMEOUT: Duration = Duration::from_secs(180);
        const STABLE_WINDOW: Duration = Duration::from_secs(6);
        const SEAL_TIMEOUT: Duration = Duration::from_secs(180);

        let recipient: Address = "0xdead000000000000000000000000000000000001".parse()?;

        // 1. Settle boot: the batcher has caught up to the RPC head (every
        //    produced block is in a sealed batch) and stays there for a window.
        let settle_deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
        let mut caught_up_since: Option<std::time::Instant> = None;
        let base = loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            // Read the batcher first: if the head advances in between, the
            // comparison below fails conservatively (we wait, never settle
            // early on a stale head).
            let BatcherProgress {
                last_included_block,
                last_sealed_batch,
            } = self.batcher_progress().await?;
            let head = self.l2_provider.get_block_number().await?;
            if head >= 1 && last_included_block == head {
                let since = *caught_up_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= STABLE_WINDOW {
                    break last_sealed_batch;
                }
            } else {
                caught_up_since = None;
            }
            anyhow::ensure!(
                std::time::Instant::now() < settle_deadline,
                "chain did not settle after boot (rpc_head={head}, batcher_block={last_included_block})"
            );
        };
        // Boot can seal exactly `target_batches` on its own; the chain is
        // then already in the requested state and no transfer is needed.
        anyhow::ensure!(
            base <= target_batches,
            "boot produced {base} batches, more than the {target_batches}-batch target"
        );

        // 2. One transfer per remaining batch, gated on each seal.
        for batch in (base + 1)..=target_batches {
            self.l2_provider
                .send_transaction(
                    TransactionRequest::default()
                        .with_to(recipient)
                        .with_value(U256::from(batch)),
                )
                .await?
                .get_receipt()
                .await?;
            let seal_deadline = std::time::Instant::now() + SEAL_TIMEOUT;
            while self.batcher_progress().await?.last_sealed_batch < batch {
                anyhow::ensure!(
                    std::time::Instant::now() < seal_deadline,
                    "batch {batch} did not seal within the deadline"
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }

        // 3. The chain must rest at exactly `target_batches`: give any stray
        //    late seal a window to surface, then require exactly the target.
        tokio::time::sleep(STABLE_WINDOW).await;
        let sealed = self.batcher_progress().await?.last_sealed_batch;
        anyhow::ensure!(
            sealed == target_batches,
            "expected exactly {target_batches} sealed batches, batcher is at {sealed}"
        );
        Ok(())
    }

    /// Drive an L1→L2 ETH deposit (priority transaction) for `beneficiary`
    /// and return the canonical L2 transaction hash once the L1 side has
    /// landed. Callers wait for the L2 receipt themselves.
    pub async fn deposit_l1_to_l2(
        &self,
        beneficiary: Address,
        amount: U256,
    ) -> anyhow::Result<B256> {
        let chain_id = self.l2_provider.get_chain_id().await?;
        let bridgehub = Bridgehub::new(
            self.l2_zk_provider.get_bridgehub_contract().await?,
            self.l1.provider.clone(),
            chain_id,
        );
        let max_priority_fee_per_gas = self.l1.provider.get_max_priority_fee_per_gas().await?;
        let base_l1_fees = self
            .l1
            .provider
            .estimate_eip1559_fees_with(Eip1559Estimator::new(|base_fee_per_gas, _| {
                alloy::eips::eip1559::Eip1559Estimation {
                    max_fee_per_gas: base_fee_per_gas * 3 / 2,
                    max_priority_fee_per_gas: 0,
                }
            }))
            .await?;
        let max_fee_per_gas = base_l1_fees.max_fee_per_gas + max_priority_fee_per_gas;
        let gas_limit = self
            .l2_provider
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

        let receipt = self
            .l1
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
            .expect("no L1->L2 logs produced by deposit tx");
        Ok(l1_to_l2_tx_log.inner.txHash)
    }

    /// Deposit L1 ETH to the test wallet if its L2 balance is zero.
    async fn ensure_test_wallet_funded(&self) -> anyhow::Result<()> {
        let beneficiary = self.l2_wallet.default_signer().address();
        let balance = self.l2_provider.get_balance(beneficiary).await?;
        if balance > U256::ZERO {
            return Ok(());
        }

        let amount = U256::from(1_000_000_000_000_000_000u128) * U256::from(1_000u64);
        let l2_tx_hash = self.deposit_l1_to_l2(beneficiary, amount).await?;

        PendingTransactionBuilder::new(self.l2_zk_provider.root().clone(), l2_tx_hash)
            .get_receipt()
            .await?;

        (|| async {
            let balance = self.l2_provider.get_balance(beneficiary).await?;
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

    pub async fn wait_for_initial_deposit(&self) -> anyhow::Result<()> {
        tokio::time::timeout(
            Duration::from_secs(60),
            self.l2_zk_provider.wait_for_block(2),
        )
        .await
        .context("timed out waiting for block 2 (initial deposit)")??;
        self.ensure_test_wallet_funded().await
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
        Self::bind_runtime_config(&l1, tempdir.as_ref(), &mut config);
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
        // The server turns port 0 into a concrete TCP+UDP p2p port immediately before building
        // reth's network config, so the advertised ENR remains dialable without test-side probing.
        config.network_config.port = 0;
        config.network_config.secret_key = Some(zksync_os_network::rng_secret_key());
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
        let bound_ports = zksync_os_server::run::<FullDiffsState>(&runtime, config.clone())
            .instrument(node_span)
            .await;
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

        // Under CI load a freshly started Anvil can wedge (stop answering RPC for 60+s)
        // right after passing the readiness check; retrying against it is hopeless, so
        // after a few failed probes we kill it and spawn a fresh one.
        const SPAWN_ATTEMPTS: usize = 3;
        let mut last_err = None;
        for attempt in 1..=SPAWN_ATTEMPTS {
            // --slots-in-an-epoch defines what blocks are "finalized" in Anvil, last finalized block is `latest - 2 * slots_in_an_epoch`
            // so we set block time to 0.25s and slots in epoch set to 10 and finalization delays is about 10*0.25s*2=5s which is reasonable for tests.
            let provider =
                ProviderBuilder::new().connect_anvil_with_wallet_and_config(|anvil| {
                    anvil
                        .chain_id(L1_CHAIN_ID)
                        .arg("--block-time")
                        .arg("0.25")
                        .arg("--mixed-mining")
                        .arg("--load-state")
                        .arg(&l1_state_path)
                        .arg("--slots-in-an-epoch")
                        .arg("10")
                })?;
            let address = provider.inner().anvil().endpoint();

            // `AnvilInstance`'s Drop never runs if the test process is SIGKILLed or aborts,
            // which would orphan an anvil that mines (and allocates) forever. The leash kills
            // it whenever this process dies, no matter how.
            leash::attach(provider.inner().anvil().child().id(), "anvil")?;

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
            let probed = (|| async {
                tokio::time::timeout(Duration::from_secs(10), NodeProvider::new(provider.clone()))
                    .await
                    .context("timed out probing L1 node capabilities")?
                    .context("failed to probe L1 node capabilities")
            })
            .retry(
                ConstantBuilder::default()
                    .with_delay(Duration::from_millis(200))
                    .with_max_times(2),
            )
            .notify(|err: &anyhow::Error, dur: Duration| {
                tracing::info!(%err, ?dur, "retrying L1 node capability probing");
            })
            .await;

            match probed {
                Ok(provider) => {
                    return Ok(Self {
                        address,
                        provider,
                        wallet,
                        _tempdir: Arc::new(tempdir),
                    });
                }
                Err(err) => {
                    tracing::warn!(%err, attempt, "anvil unresponsive to capability probing; respawning");
                    // Dropping `provider` (the last owner of `AnvilInstance`) kills the wedged anvil.
                    last_err = Some(err);
                }
            }
        }
        Err(last_err
            .expect("SPAWN_ATTEMPTS > 0")
            .context("L1 node capability probing failed for every spawned anvil"))
    }
}

/// How the Airbender service leaves its FRI loop for the SNARK stage. The
/// loop breaks ONLY on its configured limit: with `MaxFrisPerSnark(n)` and
/// fewer than `n` FRI jobs ever arriving, the service polls FRI work forever
/// and never reaches the SNARK pick — so a run whose FRIs are already proven
/// (the multi-proof settle stage) must use `SnarkOnly`, which bounds the FRI
/// loop by time (`--max-snark-latency 1`) instead of by count.
#[cfg(feature = "prover-tests")]
pub enum AirbenderMode {
    MaxFrisPerSnark(usize),
    SnarkOnly,
}

/// Spawn the real Airbender prover service for a specific protocol version
/// (the app binary and the service release are version-specific). Returns
/// the child so the caller can await its exit or kill it — the tests that
/// run both lanes must free the GPU before the ZiSK daemon starts.
///
/// Requires `COMPACT_CRS_FILE` (path to the SNARK trusted setup).
#[cfg(feature = "prover-tests")]
pub async fn spawn_airbender_prover(
    tester: &Tester,
    protocol_version: &str,
    sequencer_urls: &[String],
    iterations: usize,
    max_fris_per_snark: usize,
) -> tokio::process::Child {
    spawn_airbender_prover_with_mode(
        tester,
        protocol_version,
        sequencer_urls,
        iterations,
        AirbenderMode::MaxFrisPerSnark(max_fris_per_snark),
    )
    .await
}

/// See [`spawn_airbender_prover`]; `mode` picks how the service's FRI loop
/// yields to the SNARK stage.
#[cfg(feature = "prover-tests")]
pub async fn spawn_airbender_prover_with_mode(
    tester: &Tester,
    protocol_version: &str,
    sequencer_urls: &[String],
    iterations: usize,
    mode: AirbenderMode,
) -> tokio::process::Child {
    let app_bin_path = match protocol_version {
        PROTOCOL_VERSION_V30_2 => utils::materialize_multiblock_batch_bin(
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

    let child = tokio::process::Command::new(&path)
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
        .args(match mode {
            AirbenderMode::MaxFrisPerSnark(n) => {
                ["--max-fris-per-snark".to_string(), n.to_string()]
            }
            // The FRI loop yields after one second whether or not FRI work
            // exists, so the SNARK pick is reached with zero FRI jobs.
            AirbenderMode::SnarkOnly => ["--max-snark-latency".to_string(), "1".to_string()],
        })
        .arg("--disable-zk")
        // Without this the prover keeps running after a panic: the wait-task below is
        // dropped on runtime shutdown without ever signalling the child.
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn prover service");
    let pid = child
        .id()
        .expect("newly spawned prover service has no process ID");
    let name = std::path::Path::new(&path)
        .file_name()
        .expect("prover binary path has no file name")
        .to_string_lossy();
    // Same rationale as for anvil: `kill_on_drop` never fires if the test process is
    // SIGKILLed or aborts.
    leash::attach(pid, &name).expect("failed to attach leash to prover service");
    child
}

#[cfg(feature = "prover-tests")]
async fn spawn_prover_service(tester: &Tester, sequencer_urls: &[String], iterations: usize) {
    #[cfg(feature = "gpu-prover-tests")]
    let zisk_sequencer_url = sequencer_urls
        .first()
        .expect("at least one sequencer URL for prover tests")
        .clone();
    #[cfg(feature = "gpu-prover-tests")]
    let second_proof_system = tester
        .config
        .prover_input_generator_config
        .second_proof_system;
    let protocol_version = tester.chain_layout.protocol_version();
    let mut child =
        spawn_airbender_prover(tester, protocol_version, sequencer_urls, iterations, 1).await;
    tokio::task::spawn(async move {
        let code = child
            .wait()
            .await
            .expect("failed to wait for prover service");
        if code.success() {
            tracing::info!("Airbender prover service finished running");
        } else {
            panic!(
                "Airbender prover service terminated with exit code {}",
                code
            );
        }

        // GPU is now free. Launch the ZiSK GPU prover to generate the second
        // proof. Both provers share a single GPU and must run sequentially.
        // A test that keeps the lane off seals no ZiSK job, so the daemon has
        // nothing to prove. This run is a smoke pass with no acceptance check:
        // the Airbender lane settled its batches before the daemon started, so
        // which batches remain above the aggregation floor is not deterministic
        // (the dedicated ZiSK tests assert acceptance).
        #[cfg(feature = "gpu-prover-tests")]
        if second_proof_system
            && zisk_gpu_artifacts_available("the ZiSK lane of the auto-spawned prover run")
        {
            run_zisk_gpu_prover(&zisk_sequencer_url, 1).await;
        }
    });
}

/// Whether this run is the one that is meant to prove the ZiSK lane. The GPU
/// CI job sets `ZISK_GPU_TESTS_REQUIRED=1`: under it a missing artifact or VK
/// is a test FAILURE, never a skip — a green job that silently skipped the
/// lane is worse than no job, and it is one unset env var away at all times.
#[cfg(feature = "gpu-prover-tests")]
pub fn zisk_gpu_tests_required() -> bool {
    std::env::var("ZISK_GPU_TESTS_REQUIRED").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Entry guard for every path that drives the real ZiSK GPU daemon. The daemon
/// needs a guest ELF and a prover binary that only a ZiSK-provisioned runner
/// holds. When they are absent this prints a loud note that names them and
/// returns `false`, so a green run on a bare runner never reads as coverage of
/// the ZiSK lane — and under [`zisk_gpu_tests_required`] it panics instead,
/// so the job that exists to prove the lane cannot skip it.
#[cfg(feature = "gpu-prover-tests")]
pub fn zisk_gpu_artifacts_available(skipped: &str) -> bool {
    if std::env::var("ZISK_ELF").is_ok() && std::env::var("ZISK_AGG_ELF").is_ok() {
        return true;
    }
    let note = format!(
        "{skipped}: ZISK_ELF or ZISK_AGG_ELF is not set, so this \
         run checked NOTHING of the ZiSK lane. Set ZISK_ELF (path to the ZiSK \
         guest ELF), ZISK_AGG_ELF (path to the ZiSK aggregator guest ELF) and \
         ZISK_PROVER_BIN (path to the zksync-os-zisk-prover-service binary) on \
         a runner that holds the ZiSK artifacts."
    );
    if zisk_gpu_tests_required() {
        panic!("ZISK_GPU_TESTS_REQUIRED is set but the artifacts are missing. {note}");
    }
    eprintln!("NOTE: {note} SKIPPED.");
    false
}

/// Run the ZiSK GPU prover service until it accepts `iterations` submissions.
///
/// This must only be called after the Airbender GPU prover has exited so both
/// provers never contend for the same GPU simultaneously.
///
/// The daemon runs in aggregated mode, which is the mode the server serves: the
/// ZiSK job manager always carries an aggregation sink when the second proof
/// system is on, so it accepts `vadcop_final` streams and rejects per-batch
/// PLONK submissions. The daemon proves each picked batch to a `vadcop_final`
/// STARK and submits the stream, and proves each formed range in the aggregator
/// guest (`ZISK_AGG_ELF`) and submits one PLONK-wrapped range proof.
/// `iterations` counts both kinds.
///
/// The exit status only reports that the daemon stopped without an error — a
/// cancelled daemon also exits 0, and the server answers a submission for a
/// batch below the aggregation floor with a success it then drops. Call
/// [`assert_zisk_lane_accepted`] to check what the server actually kept.
///
/// Required environment variables:
/// - `ZISK_PROVER_BIN` — path to `zksync-os-zisk-prover-service` binary
/// - `ZISK_BINARY` — path to `cargo-zisk` GPU binary
/// - `ZISK_ELF` — path to the ZiSK guest ELF
/// - `ZISK_AGG_ELF` — path to the ZiSK aggregator guest ELF
/// - `ZISK_PK` — path to ZiSK STARK proving key directory
/// - `ZISK_SK` — path to ZiSK PLONK proving key directory
#[cfg(feature = "gpu-prover-tests")]
pub async fn run_zisk_gpu_prover(sequencer_url: &str, iterations: usize) {
    let zisk_bin = std::env::var("ZISK_PROVER_BIN")
        .unwrap_or_else(|_| "zksync-os-zisk-prover-service".to_string());
    let cargo_zisk = std::env::var("ZISK_BINARY").unwrap_or_else(|_| "cargo-zisk".to_string());
    let elf_path = std::env::var("ZISK_ELF")
        .expect("ZISK_ELF must be set for gpu-prover-tests (path to ZiSK guest ELF)");
    let agg_elf_path = std::env::var("ZISK_AGG_ELF")
        .expect("ZISK_AGG_ELF must be set (path to the aggregator guest ELF)");
    let proving_key = std::env::var("ZISK_PK")
        .unwrap_or_else(|_| format!("{}/.zisk/provingKey", std::env::var("HOME").unwrap()));
    let proving_key_plonk = std::env::var("ZISK_SK")
        .unwrap_or_else(|_| format!("{}/.zisk/provingKeySnark", std::env::var("HOME").unwrap()));

    tracing::info!(
        zisk_bin = %zisk_bin,
        cargo_zisk = %cargo_zisk,
        elf_path = %elf_path,
        agg_elf_path = %agg_elf_path,
        iterations,
        "Launching ZiSK GPU prover"
    );

    let mut child = tokio::process::Command::new(&zisk_bin)
        .arg("--sequencer-url")
        .arg(sequencer_url)
        .arg("--zisk-binary")
        .arg(&cargo_zisk)
        .arg("--elf-path")
        .arg(&elf_path)
        .arg("--proving-key")
        .arg(&proving_key)
        .arg("--proving-key-plonk")
        .arg(&proving_key_plonk)
        .arg("--aggregation")
        .arg("--aggregator-elf")
        .arg(&agg_elf_path)
        .arg("--iterations")
        .arg(iterations.to_string())
        .spawn()
        .expect("failed to spawn ZiSK prover service");

    // Bounded wait: the daemon exits on its own after `iterations` accepted
    // submissions, and a per-batch proof or an aggregation takes ~1-2 minutes
    // on the CI GPU runner. Without a deadline a wedged daemon stage would
    // announce itself only at the harness's hours-long cap.
    let code = tokio::time::timeout(std::time::Duration::from_secs(1800), child.wait())
        .await
        .unwrap_or_else(|_| {
            panic!("ZiSK GPU prover service did not finish {iterations} submissions within 1800s")
        })
        .expect("failed to wait for ZiSK prover service");
    if code.success() {
        tracing::info!("ZiSK GPU prover service finished running");
    } else {
        panic!("ZiSK GPU prover service terminated with exit code {}", code);
    }
}

/// The ZiSK lane's server-side view: what each stage of the lane holds.
///
/// The wire shape is the server's, so it is declared here rather than exported
/// from the binary — an assertion about an HTTP response belongs to the test.
#[cfg(feature = "gpu-prover-tests")]
#[derive(Debug, serde::Deserialize)]
pub struct ZiskLaneStatusPayload {
    pub per_batch: zisk_prover_lane::ZiskQueueCounts,
    pub aggregation: zisk_prover_lane::ZiskAggregationCounts,
}

/// Read the ZiSK lane status endpoint.
#[cfg(feature = "gpu-prover-tests")]
pub async fn zisk_lane_status(prover_api_url: &str) -> anyhow::Result<ZiskLaneStatusPayload> {
    let response = reqwest::Client::new()
        .get(format!("{prover_api_url}/prover-jobs/v1/ZiSK/status"))
        .send()
        .await?
        .error_for_status()?;
    Ok(response.json().await?)
}

/// Assert that the server accepted and kept the expected ZiSK submissions.
///
/// This is the acceptance check for a GPU daemon run: an accepted per-batch
/// proof stays parked as a completion marker, and an accepted range proof stays
/// parked for the MultiProof rendezvous. A submission the server answered with
/// a success but dropped — a batch below the aggregation floor, whose Airbender
/// range already settled — leaves neither, so it fails here.
#[cfg(feature = "gpu-prover-tests")]
pub async fn assert_zisk_lane_accepted(
    prover_api_url: &str,
    batch_proofs: u64,
    range_proofs: u64,
) -> anyhow::Result<()> {
    let status = zisk_lane_status(prover_api_url).await?;
    anyhow::ensure!(
        status.per_batch.proofs_completed == batch_proofs
            && status.aggregation.range_proofs_completed == range_proofs,
        "the ZiSK lane holds {} accepted per-batch proofs and {} accepted range proofs, \
         expected {batch_proofs} and {range_proofs}; full lane status: {status:?}",
        status.per_batch.proofs_completed,
        status.aggregation.range_proofs_completed,
    );
    Ok(())
}

/// Poll until the Airbender SNARK lane has registered `ranges` aggregation
/// ranges — the ZiSK lane learns a range's bounds when a real SNARK job is
/// picked, so this resolves once the Airbender lane reaches the SNARK stage and
/// the ZiSK daemon has ranges to prove.
#[cfg(feature = "gpu-prover-tests")]
pub async fn wait_for_zisk_aggregation_ranges(
    prover_api_url: &str,
    ranges: u64,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let status = zisk_lane_status(prover_api_url).await?;
        let tracked =
            status.aggregation.ranges_in_flight + status.aggregation.range_proofs_completed;
        if tracked >= ranges {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the Airbender SNARK lane registered {tracked} aggregation ranges within {timeout:?}, \
             expected {ranges}; full lane status: {status:?}"
        );
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

#[cfg(feature = "prover-tests")]
fn prover_release_for_protocol(protocol_version: &str) -> &'static str {
    match protocol_version {
        PROTOCOL_VERSION_V30_2 => "v0.7.1",
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
