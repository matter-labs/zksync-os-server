use crate::dyn_wallet_provider::EthDynProvider;
use crate::network::Zksync;
use crate::prover_tester::ProverTester;
use crate::utils::LockedPort;
use alloy::network::{EthereumWallet, TxSigner};
use alloy::primitives::{Address, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder, WalletProvider};
use alloy::signers::local::LocalSigner;
use backon::ConstantBuilder;
use backon::Retryable;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zksync_os_object_store::{ObjectStoreConfig, ObjectStoreMode};
use zksync_os_server::config::{
    BatchVerificationConfig, Config, FakeFriProversConfig, FakeSnarkProversConfig, GeneralConfig,
    GenesisConfig, ProverApiConfig, ProverInputGeneratorConfig, RpcConfig, SequencerConfig,
    StatusServerConfig,
};
use zksync_os_state_full_diffs::FullDiffsState;

pub mod assert_traits;
pub mod contracts;
pub mod dyn_wallet_provider;
mod network;
mod prover_tester;
pub mod provider;
pub mod upgrade;
mod utils;

/// L1 chain id as expected by contracts deployed in `zkos-l1-state.json`
const L1_CHAIN_ID: u64 = 31337;
pub const BATCH_VERIFICATION_KEY_1: &str =
    "0x7094f4b57ed88624583f68d2f241858f7dafb6d2558bc22d18991690d36b4e47";
pub const BATCH_VERIFICATION_ADDR_1: &str = "0xdF3401331FeB729f138258bAC135359f3CBA6760";
pub const BATCH_VERIFICATION_KEY_2: &str =
    "0xf9306dd03807c08b646d47c739bd51e4d2a25b02bad0efb3d93f095982ac98cd";
pub const BATCH_VERIFICATION_ADDR_2: &str = "0x7A255d9331f3D4ca05cD0f16c3BCC08D682ac62B";

#[derive(Debug)]
pub struct Tester {
    pub l1_provider: EthDynProvider,
    pub l2_provider: EthDynProvider,

    /// ZKsync OS-specific provider. Generally prefer to use `l2_provider` as we strive for the
    /// system to be Ethereum-compatible. But this can be useful if you need to assert custom fields
    /// that are only present in ZKsync OS response types (`l2ToL1Logs`, `commitTx`, etc).
    pub l2_zk_provider: DynProvider<Zksync>,

    pub l1_wallet: EthereumWallet,
    pub l2_wallet: EthereumWallet,

    pub prover_tester: ProverTester,

    stop_sender: watch::Sender<bool>,
    main_task: JoinHandle<()>,

    #[allow(dead_code)]
    tempdir: Arc<tempfile::TempDir>,
    main_node_tempdir: Arc<tempfile::TempDir>,

    // Needed to be able to connect external nodes
    l1_address: String,
    replay_url: String,
    l2_rpc_address: String,
    batch_verification_address: String,
}

impl Tester {
    pub fn builder() -> TesterBuilder {
        TesterBuilder::default()
    }

    pub async fn setup() -> anyhow::Result<Self> {
        Self::builder().build().await
    }

    pub async fn launch_external_node(&self) -> anyhow::Result<Self> {
        // Due to type inference issue, we need to specify None type here and this whole function if a de-facto helper for this
        self.launch_external_node_overrides(None::<fn(&mut Config)>)
            .await
    }

    pub async fn launch_external_node_overrides(
        &self,
        config_overrides: Option<impl FnOnce(&mut Config)>,
    ) -> anyhow::Result<Self> {
        let overrides_fun = |config: &mut Config| {
            config.sequencer_config.block_replay_download_address = Some(self.replay_url.clone());
            config.general_config.main_node_rpc_url = Some(self.l2_rpc_address.clone());
            config.batch_verification_config.connect_address =
                self.batch_verification_address.clone();
            if let Some(f) = config_overrides {
                f(config)
            }
        };

        Self::launch_node(
            self.l1_address.clone(),
            self.l1_provider.clone(),
            self.l1_wallet.clone(),
            false,
            Some(overrides_fun),
            Some(self.main_node_tempdir.clone()),
        )
        .await
    }

    async fn launch_node(
        l1_address: String,
        l1_provider: EthDynProvider,
        l1_wallet: EthereumWallet,
        enable_prover: bool,
        config_overrides: Option<impl FnOnce(&mut Config)>,
        main_node_tempdir: Option<Arc<tempfile::TempDir>>,
    ) -> anyhow::Result<Self> {
        (|| async {
            // Wait for L1 node to get up and be able to respond.
            l1_provider.clone().get_chain_id().await?;
            Ok(())
        })
        .retry(
            ConstantBuilder::default()
                .with_delay(Duration::from_secs(1))
                .with_max_times(10),
        )
        .notify(|err: &anyhow::Error, dur: Duration| {
            tracing::info!(%err, ?dur, "retrying connection to L1 node");
        })
        .await?;

        // Initialize and **hold** locked ports for the duration of node initialization.
        let l2_locked_port = LockedPort::acquire_unused().await?;
        let prover_api_locked_port = LockedPort::acquire_unused().await?;
        let replay_locked_port = LockedPort::acquire_unused().await?;
        let status_locked_port = LockedPort::acquire_unused().await?;
        let batch_verification_locked_port = LockedPort::acquire_unused().await?;
        let l2_rpc_address = format!("0.0.0.0:{}", l2_locked_port.port);
        let l2_rpc_ws_url = format!("ws://localhost:{}", l2_locked_port.port);
        let prover_api_address = format!("0.0.0.0:{}", prover_api_locked_port.port);
        let replay_address = format!("0.0.0.0:{}", replay_locked_port.port);
        let status_address = format!("0.0.0.0:{}", status_locked_port.port);
        let batch_verification_address =
            format!("localhost:{}", batch_verification_locked_port.port);
        let replay_url = format!("localhost:{}", replay_locked_port.port);

        let tempdir = tempfile::tempdir()?;
        let rocks_db_path = tempdir.path().join("rocksdb");
        let object_store_path = main_node_tempdir
            .as_ref()
            .map(|t| t.path())
            .unwrap_or(tempdir.path())
            .join("object_store");
        let (stop_sender, stop_receiver) = watch::channel(false);

        // Create a handle to run the sequencer in the background
        let general_config = GeneralConfig {
            rocks_db_path: rocks_db_path.clone(),
            l1_rpc_url: l1_address.clone(),
            ..Default::default()
        };
        let sequencer_config = SequencerConfig {
            block_replay_server_address: replay_address.clone(),
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
            object_store: ObjectStoreConfig {
                mode: ObjectStoreMode::FileBacked {
                    file_backed_base_path: object_store_path.clone(),
                },
                max_retries: 1,
                local_mirror_path: None,
            },
            ..Default::default()
        };

        let batch_verification_config = BatchVerificationConfig {
            server_enabled: false,
            listen_address: batch_verification_address.clone(),
            client_enabled: false,
            connect_address: batch_verification_address.clone(),
            threshold: 1, // default to 1 of 2
            accepted_signers: vec![
                BATCH_VERIFICATION_ADDR_1.into(),
                BATCH_VERIFICATION_ADDR_2.into(),
            ],
            request_timeout: Duration::from_millis(100),
            retry_delay: Duration::from_millis(10),
            total_timeout: Duration::from_secs(300),
            signing_key: BATCH_VERIFICATION_KEY_1.into(),
        };

        let status_server_config = StatusServerConfig {
            address: status_address,
        };

        let mut config = Config {
            general_config,
            genesis_config: GenesisConfig {
                genesis_input_path: Some(
                    concat!(env!("WORKSPACE_DIR"), "/genesis/genesis.json").into(),
                ),
                ..Default::default()
            },
            rpc_config,
            mempool_config: Default::default(),
            tx_validator_config: Default::default(),
            sequencer_config,
            l1_sender_config: Default::default(),
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
        };
        if let Some(f) = config_overrides {
            f(&mut config)
        }

        let main_task = tokio::task::spawn(async move {
            zksync_os_server::run::<FullDiffsState>(stop_receiver, config).await;
        });

        #[cfg(feature = "prover-tests")]
        if enable_prover {
            let base_url = format!("http://localhost:{}", prover_api_locked_port.port);
            let app_bin_path =
                zksync_os_multivm::apps::v5::multiblock_batch_path(&rocks_db_path.join("app_bins"));
            let trusted_setup_file = std::env::var("COMPACT_CRS_FILE").unwrap();
            let output_dir = tempdir.path().join("outputs");
            std::fs::create_dir_all(&output_dir).unwrap();
            tokio::task::spawn(async move {
                zksync_os_prover_service::run(zksync_os_prover_service::Args {
                    sequencer_urls: vec![base_url.parse().unwrap()],
                    app_bin_path: Some(app_bin_path),
                    circuit_limit: 10000,
                    output_dir: output_dir.to_str().unwrap().to_string(),
                    trusted_setup_file: trusted_setup_file.to_string(),
                    iterations: Some(1),
                    fri_path: None,
                    max_snark_latency: None,
                    max_fris_per_snark: Some(1),
                    disable_zk: true,
                })
                .await
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
                .with_delay(Duration::from_secs(1))
                .with_max_times(10),
        )
        .notify(|err: &anyhow::Error, dur: Duration| {
            tracing::info!(%err, ?dur, "retrying connection to L2 node");
        })
        .await?;

        // Wait for all L1 priority transaction to get executed and for our L2 account to become rich
        (|| async {
            let balance = l2_provider
                .get_balance(l2_wallet.default_signer().address())
                .await?;
            if balance == U256::ZERO {
                anyhow::bail!("L2 rich wallet balance is zero")
            }
            Ok(())
        })
        .retry(
            ConstantBuilder::default()
                .with_delay(Duration::from_secs(1))
                .with_max_times(10),
        )
        .notify(|err: &anyhow::Error, dur: Duration| {
            tracing::info!(%err, ?dur, "waiting for L2 account to become rich");
        })
        .await?;

        let l2_zk_provider = ProviderBuilder::new_with_network::<Zksync>()
            .wallet(l2_wallet.clone())
            .connect(&l2_rpc_ws_url)
            .await?;

        let tempdir = Arc::new(tempdir);
        Ok(Tester {
            l1_provider: EthDynProvider::new(l1_provider.clone()),
            l2_provider: EthDynProvider::new(l2_provider.clone()),
            l2_zk_provider: DynProvider::new(l2_zk_provider.clone()),
            l1_wallet,
            l2_wallet,
            prover_tester: ProverTester::new(
                EthDynProvider::new(l1_provider.clone()),
                EthDynProvider::new(l2_provider.clone()),
                DynProvider::new(l2_zk_provider.clone()),
            ),
            stop_sender,
            main_task,
            l1_address,
            l2_rpc_address: l2_rpc_address.replace("0.0.0.0:", "http://localhost:"),
            batch_verification_address,
            replay_url,
            tempdir: tempdir.clone(),
            main_node_tempdir: main_node_tempdir.unwrap_or(tempdir),
        })
    }
}

pub struct TesterBatchVerificationConfig {
    pub threshold: usize,
    pub request_timeout: Duration,
    pub retry_delay: Duration,
    pub total_timeout: Duration,
}

#[derive(Default)]
pub struct TesterBuilder {
    enable_prover: bool,
    block_time: Option<Duration>,
    batch_verification: Option<TesterBatchVerificationConfig>,
}

impl TesterBuilder {
    #[cfg(feature = "prover-tests")]
    pub fn enable_prover(mut self) -> Self {
        self.enable_prover = true;
        self
    }

    pub fn block_time(mut self, block_time: Duration) -> Self {
        self.block_time = Some(block_time);
        self
    }

    pub fn batch_verification(
        mut self,
        batch_verification_config: TesterBatchVerificationConfig,
    ) -> Self {
        self.batch_verification = Some(batch_verification_config);
        self
    }

    pub async fn build(self) -> anyhow::Result<Tester> {
        let l1_locked_port = LockedPort::acquire_unused().await?;
        let l1_address = format!("http://localhost:{}", l1_locked_port.port);

        let l1_provider = ProviderBuilder::new().connect_anvil_with_wallet_and_config(|anvil| {
            anvil
                .port(l1_locked_port.port)
                .chain_id(L1_CHAIN_ID)
                .arg("--load-state")
                .arg(concat!(env!("WORKSPACE_DIR"), "/zkos-l1-state.json"))
        })?;

        let l1_wallet = l1_provider.wallet().clone();

        let overrides_fun = move |config: &mut Config| {
            if let Some(block_time) = self.block_time {
                config.sequencer_config.block_time = block_time;
            }
            if let Some(batch_verification) = self.batch_verification {
                config.batch_verification_config.server_enabled = true;
                config.batch_verification_config.threshold = batch_verification.threshold;
                config.batch_verification_config.request_timeout =
                    batch_verification.request_timeout;
                config.batch_verification_config.retry_delay = batch_verification.retry_delay;
                config.batch_verification_config.total_timeout = batch_verification.total_timeout;
            }
        };

        Tester::launch_node(
            l1_address,
            EthDynProvider::new(l1_provider),
            l1_wallet,
            self.enable_prover,
            Some(overrides_fun),
            None,
        )
        .await
    }
}

impl Drop for Tester {
    fn drop(&mut self) {
        // Send stop signal to main node
        self.stop_sender.send(true).unwrap();
        self.main_task.abort();
    }
}
