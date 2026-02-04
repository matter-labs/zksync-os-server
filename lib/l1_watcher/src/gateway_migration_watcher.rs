use tokio::sync::mpsc;

use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event};
use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Log;
use zksync_os_contract_interface::{ServerNotifier::MigrateToGateway, ZkChain};
use zksync_os_types::SystemTxEnvelope;

pub struct GatewayMigrationWatcher {
    server_notifier_contract: Address,
    output: mpsc::Sender<SystemTxEnvelope>,
}

impl GatewayMigrationWatcher {
    pub async fn create_watcher(
        zk_chain: ZkChain<DynProvider>,
        config: L1WatcherConfig,
        output: mpsc::Sender<SystemTxEnvelope>,
    ) -> anyhow::Result<L1Watcher> {
        let this = Self {
            server_notifier_contract: zk_chain.get_server_notifier_address().await?,
            output,
        };

        // todo: need to make correct way
        let next_l1_block = zk_chain.provider().get_block_number().await?;
        let l1_watcher = L1Watcher::new(
            zk_chain.provider().clone(),
            next_l1_block,
            config.max_blocks_to_process,
            config.poll_interval,
            this.into(),
        );
        Ok(l1_watcher)
    }
}

#[async_trait::async_trait]
impl ProcessL1Event for GatewayMigrationWatcher {
    const NAME: &'static str = "gateway_migration";

    type SolEvent = MigrateToGateway;
    type WatchedEvent = MigrateToGateway;

    fn contract_address(&self) -> Address {
        self.server_notifier_contract
    }

    async fn process_event(
        &mut self,
        tx: MigrateToGateway,
        _log: Log,
    ) -> Result<(), L1WatcherError> {
        let envelope = SystemTxEnvelope::set_sl_chain_id(tx.chainId.try_into().unwrap());

        self.output
            .send(envelope)
            .await
            .map_err(|_| L1WatcherError::OutputClosed)
    }
}
