use crate::config::{ChainLayout, load_chain_config};
use crate::dyn_wallet_provider::EthDynProvider;
use crate::network::Zksync;
use crate::prover_tester::ProverTester;
use crate::provider::{ZksyncApi, ZksyncTestingProvider};
use crate::utils::LockedPort;
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, U256};
use alloy::providers::utils::Eip1559Estimator;
use alloy::providers::{
    DynProvider, Identity, PendingTransactionBuilder, Provider, ProviderBuilder, WalletProvider,
};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::{LocalSigner, PrivateKeySigner};
use anyhow::Context;
use backon::ConstantBuilder;
use backon::Retryable;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_network::NodeRecord;
pub use zksync_os_server::config::DeploymentFilterConfig;
use zksync_os_server::config::{
    BatchVerificationConfig, Config, FakeFriProversConfig, FakeSnarkProversConfig, FeeConfig,
    GeneralConfig, NetworkConfig, ProofStorageConfig, ProverApiConfig, ProverInputGeneratorConfig,
    RpcConfig, SequencerConfig, StatusServerConfig,
};
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
mod prover_tester;
pub mod provider;
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

    pub const fn next_to_l1() -> Self {
        Self {
            protocol_version: NEXT_PROTOCOL_VERSION,
            settlement_layer: SettlementLayer::L1,
        }
    }

    pub const fn next_to_gateway() -> Self {
        Self {
            protocol_version: NEXT_PROTOCOL_VERSION,
            settlement_layer: SettlementLayer::Gateway,
        }
    }

    pub fn builder(self) -> TesterBuilder {
        Tester::builder()
            .protocol_version(self.protocol_version)
            .settlement_layer(self.settlement_layer)
    }

    pub async fn setup(self) -> anyhow::Result<Tester> {
        self.builder().build().await
    }
}

pub const CURRENT_TO_L1: TestCase = TestCase::current_to_l1();
pub const NEXT_TO_L1: TestCase = TestCase::next_to_l1();
pub const NEXT_TO_GATEWAY: TestCase = TestCase::next_to_gateway();

/// Set of private keys for batch verification participants.
pub const BATCH_VERIFICATION_KEYS: [&str; 2] = [
    "0x7094f4b57ed88624583f68d2f241858f7dafb6d2558bc22d18991690d36b4e47",
    "0xf9306dd03807c08b646d47c739bd51e4d2a25b02bad0efb3d93f095982ac98cd",
];
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

#[derive(Debug)]
pub struct Tester {
    pub l1: AnvilL1,

    pub l2_provider: EthDynProvider,
    /// ZKsync OS-specific provider. Generally prefer to use `l2_provider` as we strive for the
    /// system to be Ethereum-compatible. But this can be useful if you need to assert custom fields
    /// that are only present in ZKsync OS response types (`l2ToL1Logs`, `commitTx`, etc).
    pub l2_zk_provider: DynProvider<Zksync>,
    pub l2_wallet: EthereumWallet,

    pub prover_tester: ProverTester,

    stop_sender: watch::Sender<bool>,
    main_task: JoinHandle<()>,

    #[allow(dead_code)]
    tempdir: Arc<tempfile::TempDir>,

    // Needed to be able to connect external nodes
    node_record: NodeRecord,
    l2_rpc_address: String,
    batch_verification_url: String,
    gateway_rpc_url: Option<String>,
    sl_provider: EthDynProvider,
    chain_layout: ChainLayout<'static>,
    supporting_nodes: Vec<Tester>,
}

impl Tester {
    pub fn l1_provider(&self) -> &EthDynProvider {
        &self.l1.provider
    }

    pub fn l1_wallet(&self) -> &EthereumWallet {
        &self.l1.wallet
    }

    pub fn sl_provider(&self) -> &EthDynProvider {
        &self.sl_provider
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
}

impl Tester {
    pub fn builder() -> TesterBuilder {
        TesterBuilder::default()
    }

    pub async fn setup() -> anyhow::Result<Self> {
        Self::builder().build().await
    }

    pub fn l2_rpc_url(&self) -> &str {
        &self.l2_rpc_address
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
        let overrides_fun = |config: &mut Config| {
            config.general_config.node_role = NodeRole::ExternalNode;
            config.network_config.boot_nodes = vec![self.node_record];
            config.general_config.main_node_rpc_url = Some(self.l2_rpc_address.clone());
            config.l1_sender_config.pubdata_mode = None;
            config.general_config.gateway_rpc_url = self.gateway_rpc_url.clone();
            config.batch_verification_config.connect_address = self.batch_verification_url.clone();
            if let Some(f) = config_overrides {
                f(config)
            }
        };

        Self::launch_node(
            self.l1.clone(),
            false,
            Some(overrides_fun),
            self.chain_layout,
        )
        .await
    }

    async fn launch_node(
        l1: AnvilL1,
        enable_prover: bool,
        config_overrides: Option<impl FnOnce(&mut Config)>,
        chain_layout: ChainLayout<'static>,
    ) -> anyhow::Result<Self> {
        // Initialize and **hold** locked ports for the duration of node initialization.
        let l2_locked_port = LockedPort::acquire_unused().await?;
        let prover_api_locked_port = LockedPort::acquire_unused().await?;
        let network_locked_port = LockedPort::acquire_unused().await?;
        let status_locked_port = LockedPort::acquire_unused().await?;
        let batch_verification_locked_port = LockedPort::acquire_unused().await?;
        let l2_rpc_address = format!("0.0.0.0:{}", l2_locked_port.port);
        let l2_rpc_ws_url = format!("ws://localhost:{}", l2_locked_port.port);
        let prover_api_address = format!("0.0.0.0:{}", prover_api_locked_port.port);
        let status_address = format!("0.0.0.0:{}", status_locked_port.port);
        let batch_verification_address = format!("0.0.0.0:{}", batch_verification_locked_port.port);
        let batch_verification_url =
            format!("http://localhost:{}", batch_verification_locked_port.port);

        let tempdir = tempfile::tempdir()?;
        let rocks_db_path = tempdir.path().join("rocksdb");
        // ENs will not use this dir
        let proof_storage_path = tempdir.path().join("proof_storage_path");
        let (stop_sender, stop_receiver) = watch::channel(false);

        let default_config = load_chain_config(chain_layout);

        // Create a handle to run the sequencer in the background
        let general_config = GeneralConfig {
            rocks_db_path: rocks_db_path.clone(),
            l1_rpc_url: l1.address.clone(),
            ..default_config.general_config
        };
        let sequencer_config = SequencerConfig {
            fee_collector_address: Address::random(),
            ..Default::default()
        };
        let rpc_config = RpcConfig {
            address: l2_rpc_address.clone(),
            // Override default with a higher value as the test can be slow in CI
            send_raw_transaction_sync_timeout: Duration::from_secs(10),
            ..Default::default()
        };
        let prover_api_config = ProverApiConfig {
            fake_fri_provers: FakeFriProversConfig {
                enabled: !enable_prover,
                ..Default::default()
            },
            fake_snark_provers: FakeSnarkProversConfig {
                enabled: !enable_prover,
                ..Default::default()
            },
            address: prover_api_address,
            proof_storage: ProofStorageConfig {
                path: proof_storage_path.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        let batch_verification_config = BatchVerificationConfig {
            server_enabled: false,
            listen_address: batch_verification_address.clone(),
            client_enabled: false,
            connect_address: batch_verification_url.clone(),
            threshold: 1, // default to 1 of 2
            accepted_signers: BATCH_VERIFICATION_ADDRESSES.clone(),
            request_timeout: Duration::from_millis(500),
            retry_delay: Duration::from_secs(1),
            total_timeout: Duration::from_secs(300),
            signing_key: BATCH_VERIFICATION_KEYS[0].into(),
        };

        let status_server_config = StatusServerConfig {
            enabled: true,
            address: status_address,
        };

        let network_secret_key = zksync_os_network::rng_secret_key();
        let node_record = NodeRecord::from_secret_key(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), network_locked_port.port),
            &network_secret_key,
        );
        let network_config = NetworkConfig {
            enabled: true,
            secret_key: Some(network_secret_key),
            address: Ipv4Addr::LOCALHOST,
            port: network_locked_port.port,
            boot_nodes: vec![],
        };

        let mut config = Config {
            general_config,
            network_config,
            genesis_config: default_config.genesis_config.clone(),
            rpc_config,
            mempool_config: Default::default(),
            tx_validator_config: Default::default(),
            sequencer_config,
            l1_sender_config: default_config.l1_sender_config.clone(),
            l1_watcher_config: Default::default(),
            batcher_config: Default::default(),
            prover_input_generator_config: ProverInputGeneratorConfig {
                logging_enabled: enable_prover,
                ..Default::default()
            },
            prover_api_config,
            status_server_config,
            observability_config: Default::default(),
            gas_adjuster_config: Default::default(),
            batch_verification_config,
            base_token_price_updater_config: default_config.base_token_price_updater_config.clone(),
            interop_fee_updater_config: default_config.interop_fee_updater_config.clone(),
            external_price_api_client_config: default_config
                .external_price_api_client_config
                .clone(),
            fee_config: Default::default(),
        };

        if let Some(ephemeral_state) = &config.general_config.ephemeral_state {
            tracing::info!("Loading ephemeral state from {}", ephemeral_state.display());
            zksync_os_server::util::unpack_ephemeral_state(
                ephemeral_state,
                &config.general_config.rocks_db_path,
            );
        }
        if let Some(f) = config_overrides {
            f(&mut config)
        }
        let gateway_rpc_url = config.general_config.gateway_rpc_url.clone();

        let main_task = tokio::task::spawn(async move {
            zksync_os_server::run::<FullDiffsState>(stop_receiver, config).await;
        });

        #[cfg(feature = "prover-tests")]
        if enable_prover {
            let base_url = format!("http://localhost:{}", prover_api_locked_port.port);
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

        // Deposits fail before genesis upgrade tx is processed, so we wait for the first block with upgrade tx.
        l2_zk_provider.wait_for_block(1).await?;
        ensure_test_wallet_funded(
            &l1,
            &EthDynProvider::new(l2_provider.clone()),
            &DynProvider::new(l2_zk_provider.clone()),
            &l2_wallet,
        )
        .await?;

        let tempdir = Arc::new(tempdir);
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
        let prover_tester = ProverTester::new(
            EthDynProvider::new(l1.provider.clone()),
            sl_provider.clone(),
            EthDynProvider::new(l2_provider.clone()),
            DynProvider::new(l2_zk_provider.clone()),
        );
        Ok(Tester {
            l1,
            l2_provider: EthDynProvider::new(l2_provider.clone()),
            l2_zk_provider: DynProvider::new(l2_zk_provider.clone()),
            l2_wallet,
            prover_tester,
            stop_sender,
            main_task,
            l2_rpc_address: l2_rpc_address.replace("0.0.0.0:", "http://localhost:"),
            batch_verification_url,
            gateway_rpc_url,
            sl_provider,
            node_record,
            tempdir: tempdir.clone(),
            chain_layout,
            supporting_nodes: Vec::new(),
        })
    }
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

#[derive(Clone, Default)]
struct NodeBuilderOptions {
    enable_prover: bool,
    block_time: Option<Duration>,
    batch_verification_threshold: Option<u64>,
    fee_config: Option<FeeConfig>,
    gas_price_scale_factor: Option<f64>,
    estimate_gas_pubdata_price_factor: Option<f64>,
}

impl NodeBuilderOptions {
    fn apply_to_config(&self, config: &mut Config) {
        if let Some(block_time) = self.block_time {
            config.sequencer_config.block_time = block_time;
        }
        if let Some(batch_verification_threshold) = self.batch_verification_threshold {
            config.batch_verification_config.server_enabled = true;
            config.batch_verification_config.threshold = batch_verification_threshold;
        }
        if let Some(fee_config) = self.fee_config.clone() {
            config.fee_config = fee_config;
        }
        if let Some(factor) = self.gas_price_scale_factor {
            config.rpc_config.gas_price_scale_factor = factor;
        }
        if let Some(factor) = self.estimate_gas_pubdata_price_factor {
            config.rpc_config.estimate_gas_pubdata_price_factor = factor;
        }
    }
}

#[derive(Clone)]
pub struct TesterBuilder {
    options: NodeBuilderOptions,
    protocol_version: &'static str,
    settlement_layer: SettlementLayer,
}

impl Default for TesterBuilder {
    fn default() -> Self {
        Self {
            options: NodeBuilderOptions::default(),
            protocol_version: PROTOCOL_VERSION,
            settlement_layer: SettlementLayer::L1,
        }
    }
}

impl TesterBuilder {
    #[cfg(feature = "prover-tests")]
    pub fn enable_prover(mut self) -> Self {
        self.options.enable_prover = true;
        self
    }

    pub fn block_time(mut self, block_time: Duration) -> Self {
        self.options.block_time = Some(block_time);
        self
    }

    pub fn batch_verification(mut self, threshold: u64) -> Self {
        self.options.batch_verification_threshold = Some(threshold);
        self
    }

    pub fn fee_config(mut self, c: FeeConfig) -> Self {
        self.options.fee_config = Some(c);
        self
    }

    pub fn gas_price_scale_factor(mut self, factor: f64) -> Self {
        self.options.gas_price_scale_factor = Some(factor);
        self
    }

    pub fn estimate_gas_pubdata_price_factor(mut self, factor: f64) -> Self {
        self.options.estimate_gas_pubdata_price_factor = Some(factor);
        self
    }

    pub fn protocol_version(mut self, protocol_version: &'static str) -> Self {
        self.protocol_version = protocol_version;
        self
    }

    pub fn settlement_layer(mut self, settlement_layer: SettlementLayer) -> Self {
        self.settlement_layer = settlement_layer;
        self
    }

    pub async fn build(self) -> anyhow::Result<Tester> {
        match self.settlement_layer {
            SettlementLayer::L1 => {
                let chain_layout = ChainLayout::Default {
                    protocol_version: self.protocol_version,
                };
                let l1 = AnvilL1::start(chain_layout).await?;
                let options = self.options;
                Tester::launch_node(
                    l1,
                    options.enable_prover,
                    Some(move |config: &mut Config| options.apply_to_config(config)),
                    chain_layout,
                )
                .await
            }
            SettlementLayer::Gateway => {
                let gateway_tester = GatewayTester::builder()
                    .protocol_version(self.protocol_version)
                    .num_chains(1)
                    .chain_options(self.options)
                    .build()
                    .await?;
                Ok(gateway_tester.into_primary_chain())
            }
        }
    }
}

impl Drop for Tester {
    fn drop(&mut self) {
        // Send stop signal to main node
        // Ignore error if receiver is already dropped (service already stopped)
        let _ = self.stop_sender.send(true);
        self.main_task.abort();
    }
}

/// Multi-chain test environment with multiple L2 chains settling to a gateway chain.
pub struct GatewayTester {
    pub l1: AnvilL1,
    pub gateway: Tester,
    pub chains: Vec<Tester>,
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

    /// Get chain A (first chain)
    pub fn chain_a(&self) -> &Tester {
        self.chain(0)
    }

    /// Get chain B (second chain)
    pub fn chain_b(&self) -> &Tester {
        self.chain(1)
    }

    pub fn gateway(&self) -> &Tester {
        &self.gateway
    }

    pub fn into_gateway(self) -> Tester {
        self.gateway
    }

    pub fn into_primary_chain(mut self) -> Tester {
        let mut chain = self.chains.remove(0);
        chain.supporting_nodes.push(self.gateway);
        chain
    }
}

pub struct GatewayTesterBuilder {
    protocol_version: &'static str,
    num_chains: Option<usize>,
    chain_options: NodeBuilderOptions,
    deployment_filter: Option<DeploymentFilterConfig>,
}

impl Default for GatewayTesterBuilder {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION_V31_0,
            num_chains: None,
            chain_options: NodeBuilderOptions::default(),
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

    fn chain_options(mut self, chain_options: NodeBuilderOptions) -> Self {
        self.chain_options = chain_options;
        self
    }

    /// Set the deployment filter config for all chains.
    pub fn deployment_filter(mut self, config: DeploymentFilterConfig) -> Self {
        self.deployment_filter = Some(config);
        self
    }

    pub async fn build(self) -> anyhow::Result<GatewayTester> {
        let num_chains = self.num_chains.unwrap_or(2);
        assert!(
            num_chains >= 1,
            "GatewayTester requires at least 1 non-gateway chain"
        );

        let protocol_version = self.protocol_version;
        let l1 = AnvilL1::start(ChainLayout::Gateway { protocol_version }).await?;
        let gateway = Tester::launch_node(
            l1.clone(),
            false,
            None::<fn(&mut Config)>,
            ChainLayout::Gateway { protocol_version },
        )
        .await?;
        let gateway_rpc_url = gateway.l2_rpc_url().to_owned();

        let mut chains = Vec::with_capacity(num_chains);
        for i in 0..num_chains {
            let chain_layout = ChainLayout::GatewayChain {
                protocol_version,
                chain_index: i,
            };
            let chain_config = load_chain_config(chain_layout);
            let chain_id = chain_config
                .genesis_config
                .chain_id
                .expect("Chain ID must be set in chain config");
            wait_for_gateway_readiness(&l1, &gateway, &chain_config).await?;
            let gateway_rpc_url = gateway_rpc_url.clone();
            let chain_options = self.chain_options.clone();
            let deployment_filter = self.deployment_filter.clone();

            let tester = Tester::launch_node(
                l1.clone(),
                chain_options.enable_prover,
                Some(move |config: &mut Config| {
                    config.general_config.gateway_rpc_url = Some(gateway_rpc_url.clone());
                    chain_options.apply_to_config(config);
                    if let Some(deployment_filter) = deployment_filter {
                        config.sequencer_config.tx_validator.deployment_filter = deployment_filter;
                    }
                }),
                chain_layout,
            )
            .await?;

            tracing::info!(
                "L2 chain {} started with chain_id {} on {}",
                i,
                chain_id,
                tester.l2_rpc_address
            );

            chains.push(tester);
        }

        Ok(GatewayTester {
            l1,
            gateway,
            chains,
        })
    }
}

async fn wait_for_gateway_readiness(
    l1: &AnvilL1,
    gateway: &Tester,
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
            .connect(gateway.l2_rpc_url())
            .await
            .with_context(|| {
                format!(
                    "failed to connect to gateway RPC at {}",
                    gateway.l2_rpc_url()
                )
            })?;

        L1State::fetch_finalized(
            DynProvider::new(l1.provider.clone()),
            DynProvider::new(gateway_provider),
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
