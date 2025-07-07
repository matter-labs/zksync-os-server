use crate::utils::LockedPort;
use alloy::network::EthereumWallet;
use alloy::providers::{DynProvider, Provider, ProviderBuilder, WalletProvider};
use alloy::signers::local::LocalSigner;
use std::str::FromStr;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zksync_os_l1_sender::config::L1SenderConfig;
use zksync_os_l1_watcher::L1WatcherConfig;
use zksync_os_sequencer::config::{ProverApiConfig, RpcConfig, SequencerConfig};

mod utils;

pub struct Tester {
    pub l1_provider: DynProvider,
    pub l2_provider: DynProvider,
    pub l1_wallet: EthereumWallet,
    pub l2_wallet: EthereumWallet,

    stop_sender: watch::Sender<bool>,
    main_task: JoinHandle<()>,
}

impl Tester {
    pub async fn setup() -> anyhow::Result<Self> {
        let l1_locked_port = LockedPort::acquire_unused().await?;
        let l1_address = format!("http://localhost:{}", l1_locked_port.port);
        let l1_provider = ProviderBuilder::new().connect_anvil_with_wallet_and_config(|anvil| {
            anvil
                .port(l1_locked_port.port)
                .chain_id(9)
                .arg("--load-state")
                .arg("../zkos-l1-state.json")
        })?;

        let l2_locked_port = LockedPort::acquire_unused().await?;
        let l2_address = format!("http://localhost:{}", l2_locked_port.port);
        let prover_api_locked_port = LockedPort::acquire_unused().await?;
        let rocksdb_path = tempfile::tempdir()?;
        let (stop_sender, stop_receiver) = watch::channel(false);
        let main_task = tokio::task::spawn(zksync_os_sequencer::run(
            stop_receiver,
            RpcConfig {
                address: format!("0.0.0.0:{}", l2_locked_port.port),
                ..Default::default()
            },
            SequencerConfig {
                rocks_db_path: rocksdb_path.path().to_path_buf(),
                ..Default::default()
            },
            L1SenderConfig {
                rocks_db_path: rocksdb_path.path().to_path_buf(),
                l1_api_url: l1_address.clone(),
                ..Default::default()
            },
            L1WatcherConfig {
                rocks_db_path: rocksdb_path.path().to_path_buf(),
                l1_api_url: l1_address.clone(),
                ..Default::default()
            },
            Default::default(),
            ProverApiConfig {
                address: format!("0.0.0.0:{}", prover_api_locked_port.port),
                ..Default::default()
            },
        ));

        // todo: wait for healthcheck endpoint instead once there is one
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let l2_wallet = EthereumWallet::new(
            // Private key for 0x36615cf349d7f6344891b1e7ca7c72883f5dc049
            LocalSigner::from_str(
                "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110",
            )
            .unwrap(),
        );
        let l2_provider = ProviderBuilder::new()
            .wallet(l2_wallet.clone())
            .connect(&l2_address)
            .await?;

        // Wait for node to get up and be able to respond.
        l2_provider.get_chain_id().await?;

        let l1_wallet = l1_provider.wallet().clone();

        Ok(Tester {
            l1_provider: DynProvider::new(l1_provider),
            l2_provider: DynProvider::new(l2_provider),
            l1_wallet,
            l2_wallet,
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
