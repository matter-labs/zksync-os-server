use crate::dyn_wallet_provider::EthDynProvider;
use crate::network::Zksync;
use crate::prover_api::ProverApi;
use crate::utils::LockedPort;
use alloy::network::{EthereumWallet, TxSigner};
use alloy::primitives::U256;
use alloy::providers::{DynProvider, Provider, ProviderBuilder, WalletProvider};
use alloy::signers::local::LocalSigner;
use backon::ConstantBuilder;
use backon::Retryable;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zksync_os_bin::config::{
    Config, FakeFriProversConfig, FakeSnarkProversConfig, GeneralConfig, GenesisConfig,
    ProverApiConfig, ProverInputGeneratorConfig, RpcConfig, SequencerConfig, StatusServerConfig,
};
use zksync_os_object_store::{ObjectStoreConfig, ObjectStoreMode};
use zksync_os_state_full_diffs::FullDiffsState;

pub mod assert_traits;
pub mod contracts;
pub mod dyn_wallet_provider;
mod network;
mod prover_api;
pub mod provider;
mod utils;

/// L1 chain id as expected by contracts deployed in `zkos-l1-state.json`
const L1_CHAIN_ID: u64 = 31337;

pub struct Tester {
    pub l1_provider: EthDynProvider,
    pub l2_provider: EthDynProvider,

    /// ZKsync OS-specific provider. Generally prefer to use `l2_provider` as we strive for the
    /// system to be Ethereum-compatible. But this can be useful if you need to assert custom fields
    /// that are only present in ZKsync OS response types (`l2ToL1Logs`, `commitTx`, etc).
    pub l2_zk_provider: DynProvider<Zksync>,

    pub l1_wallet: EthereumWallet,
    pub l2_wallet: EthereumWallet,

    pub prover_api: ProverApi,

    stop_sender: watch::Sender<bool>,
    main_task: JoinHandle<()>,

    #[allow(dead_code)]
    tempdir: Arc<tempfile::TempDir>,
    main_node_tempdir: Arc<tempfile::TempDir>,

    // Needed to be able to connect external nodes
    l1_address: String,
    replay_url: String,
}

impl Tester {
    pub fn builder() -> TesterBuilder {
        TesterBuilder::default()
    }

    pub async fn setup() -> anyhow::Result<Self> {
        Self::builder().build().await
    }

    pub async fn launch_external_node(&self) -> anyhow::Result<Self> {
        Self::launch_node(
            self.l1_address.clone(),
            self.l1_provider.clone(),
            self.l1_wallet.clone(),
            false,
            Some(self.replay_url.clone()),
            None,
            Some(self.main_node_tempdir.clone()),
        )
        .await
    }

    async fn launch_node(
        l1_address: String,
        l1_provider: EthDynProvider,
        l1_wallet: EthereumWallet,
        enable_prover: bool,
        main_node_replay_url: Option<String>,
        block_time: Option<Duration>,
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
        let l2_rpc_address = format!("0.0.0.0:{}", l2_locked_port.port);
        let l2_rpc_ws_url = format!("ws://localhost:{}", l2_locked_port.port);
        let prover_api_address = format!("0.0.0.0:{}", prover_api_locked_port.port);
        let prover_api_url = format!("http://localhost:{}", prover_api_locked_port.port);
        let replay_address = format!("0.0.0.0:{}", replay_locked_port.port);
        let status_address = format!("0.0.0.0:{}", status_locked_port.port);
        let replay_url = format!("localhost:{}", replay_locked_port.port);

        let tempdir = tempfile::tempdir()?;
        let rocks_db_path = tempdir.path().join("rocksdb");
        let app_bin_unpack_path = tempdir.path().join("apps");
        let object_store_path = main_node_tempdir
            .as_ref()
            .map(|t| t.path())
            .unwrap_or(tempdir.path())
            .join("object_store");
        let (stop_sender, stop_receiver) = watch::channel(false);

        // Create a handle to run the sequencer in the background
        let general_config = GeneralConfig {
            rocks_db_path,
            l1_rpc_url: l1_address.clone(),
            ..Default::default()
        };
        let mut sequencer_config = SequencerConfig {
            block_replay_server_address: replay_address.clone(),
            block_replay_download_address: main_node_replay_url,
            ..Default::default()
        };
        if let Some(block_time) = block_time {
            sequencer_config.block_time = block_time;
        }
        let rpc_config = RpcConfig {
            address: l2_rpc_address,
            ..Default::default()
        };
        let prover_api_config = ProverApiConfig {
            fake_fri_provers: FakeFriProversConfig {
                enabled: !enable_prover,
                ..Default::default()
            },
            fake_snark_provers: FakeSnarkProversConfig {
                enabled: true,
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

        let status_server_config = StatusServerConfig {
            address: status_address,
        };

        let config = Config {
            general_config,
            genesis_config: GenesisConfig {
                genesis_input_path: concat!(env!("WORKSPACE_DIR"), "/genesis/genesis.json").into(),
                ..Default::default()
            },
            rpc_config,
            mempool_config: Default::default(),
            sequencer_config,
            l1_sender_config: Default::default(),
            l1_watcher_config: Default::default(),
            batcher_config: Default::default(),
            prover_input_generator_config: ProverInputGeneratorConfig {
                logging_enabled: enable_prover,
                app_bin_unpack_path: app_bin_unpack_path.clone(),
                ..Default::default()
            },
            prover_api_config,
            status_server_config,
        };
        let main_task = tokio::task::spawn(async move {
            zksync_os_bin::run::<FullDiffsState>(stop_receiver, config).await;
        });

        #[cfg(feature = "prover-tests")]
        if enable_prover {
            let base_url = prover_api_url.clone();
            let app_bin_path =
                zksync_os_multivm::apps::v2::multiblock_batch_path(&app_bin_unpack_path);
            tokio::task::spawn(async move {
                zksync_os_fri_prover::run(zksync_os_fri_prover::Args {
                    base_url,
                    enabled_logging: true,
                    app_bin_path: Some(app_bin_path),
                    circuit_limit: 10000,
                    iterations: None,
                    path: None,
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
            l1_provider: EthDynProvider::new(l1_provider),
            l2_provider: EthDynProvider::new(l2_provider),
            l2_zk_provider: DynProvider::new(l2_zk_provider),
            l1_wallet,
            l2_wallet,
            prover_api: ProverApi::new(prover_api_url),
            stop_sender,
            main_task,
            l1_address,
            replay_url,
            tempdir: tempdir.clone(),
            main_node_tempdir: main_node_tempdir.unwrap_or(tempdir),
        })
    }
}

#[derive(Default)]
pub struct TesterBuilder {
    enable_prover: bool,
    block_time: Option<Duration>,
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

        Tester::launch_node(
            l1_address,
            EthDynProvider::new(l1_provider),
            l1_wallet,
            self.enable_prover,
            None,
            self.block_time,
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
