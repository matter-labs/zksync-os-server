use crate::dyn_wallet_provider::EthDynProvider;
use crate::prover_api::ProverApi;
use crate::utils::LockedPort;
use alloy::network::EthereumWallet;
use alloy::providers::{Provider, ProviderBuilder, WalletProvider};
use alloy::signers::local::LocalSigner;
use backon::ConstantBuilder;
use backon::Retryable;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zksync_os_l1_sender::config::L1SenderConfig;
use zksync_os_l1_watcher::L1WatcherConfig;
use zksync_os_sequencer::config::{
    FakeProversConfig, MempoolConfig, ProverApiConfig, ProverInputGeneratorConfig, RpcConfig,
    SequencerConfig,
};

pub mod assert_traits;
pub mod contracts;
pub mod dyn_wallet_provider;
mod prover_api;
mod utils;

/// L1 chain id as expected by contracts deployed in `zkos-l1-state.json`
const L1_CHAIN_ID: u64 = 9;

pub struct Tester {
    pub l1_provider: EthDynProvider,
    pub l2_provider: EthDynProvider,
    pub l1_wallet: EthereumWallet,
    pub l2_wallet: EthereumWallet,

    pub prover_api: ProverApi,

    stop_sender: watch::Sender<bool>,
    main_task: JoinHandle<()>,
}

impl Tester {
    pub fn builder() -> TesterBuilder {
        TesterBuilder::default()
    }

    pub async fn setup() -> anyhow::Result<Self> {
        Self::builder().build().await
    }
}

#[derive(Default)]
pub struct TesterBuilder {
    enable_prover: bool,
}

impl TesterBuilder {
    #[cfg(feature = "prover-tests")]
    pub fn enable_prover(mut self) -> Self {
        self.enable_prover = true;
        self
    }

    pub async fn build(self) -> anyhow::Result<Tester> {
        let l1_locked_port = LockedPort::acquire_unused().await?;
        let l1_address = format!("ws://localhost:{}", l1_locked_port.port);
        let l1_provider = ProviderBuilder::new().connect_anvil_with_wallet_and_config(|anvil| {
            let anvil = if std::env::var("CI").is_ok() {
                // This is where `anvil` gets installed to in our CI. For some reason it does not
                // make it into PATH. todo: investigate why
                anvil.path("/root/.foundry/bin/anvil")
            } else {
                anvil
            };
            anvil
                .port(l1_locked_port.port)
                .chain_id(L1_CHAIN_ID)
                .arg("--load-state")
                .arg("../zkos-l1-state.json")
        })?;
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
            tracing::info!(?err, ?dur, "retrying connection to L1 node");
        })
        .await?;

        let l2_locked_port = LockedPort::acquire_unused().await?;
        let l2_address = format!("ws://localhost:{}", l2_locked_port.port);
        let prover_api_locked_port = LockedPort::acquire_unused().await?;
        let rocksdb_path = tempfile::tempdir()?;
        let (stop_sender, stop_receiver) = watch::channel(false);
        // Create a handle to run the sequencer in the background
        let rpc_config = RpcConfig {
            address: format!("0.0.0.0:{}", l2_locked_port.port),
            ..Default::default()
        };
        let sequencer_config = SequencerConfig {
            rocks_db_path: rocksdb_path.path().to_path_buf(),
            ..Default::default()
        };
        let l1_sender_config = L1SenderConfig {
            l1_api_url: l1_address.clone(),
            ..Default::default()
        };
        let l1_watcher_config = L1WatcherConfig {
            rocks_db_path: rocksdb_path.path().to_path_buf(),
            l1_api_url: l1_address.clone(),
            ..Default::default()
        };
        let prover_api_config = ProverApiConfig {
            fake_provers: FakeProversConfig {
                enabled: !self.enable_prover,
                ..Default::default()
            },
            address: format!("0.0.0.0:{}", prover_api_locked_port.port),
            ..Default::default()
        };
        let main_task = tokio::task::spawn(async move {
            zksync_os_sequencer::run(
                stop_receiver,
                Default::default(),
                rpc_config,
                MempoolConfig::default(),
                sequencer_config,
                l1_sender_config,
                l1_watcher_config,
                Default::default(),
                ProverInputGeneratorConfig {
                    logging_enabled: self.enable_prover,
                    ..Default::default()
                },
                prover_api_config,
            )
            .await;
        });

        let prover_api_url = format!("http://localhost:{}", prover_api_locked_port.port);
        #[cfg(feature = "prover-tests")]
        if self.enable_prover {
            tokio::task::spawn(zkos_prover::run(zkos_prover::Args {
                base_url: prover_api_url.clone(),
                enabled_logging: true,
                app_bin_path: Some("../server_app_logging_enabled.bin".parse().unwrap()),
            }));
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
                .connect(&l2_address)
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
            tracing::info!(?err, ?dur, "retrying connection to L2 node");
        })
        .await?;

        let l1_wallet = l1_provider.wallet().clone();

        Ok(Tester {
            l1_provider: EthDynProvider::new(l1_provider),
            l2_provider: EthDynProvider::new(l2_provider),
            l1_wallet,
            l2_wallet,
            prover_api: ProverApi::new(prover_api_url),
            stop_sender,
            main_task,
        })
    }
}

impl Drop for Tester {
    fn drop(&mut self) {
        // Send stop signal to main node
        self.stop_sender.send(true).unwrap();
        self.main_task.abort();
    }
}
