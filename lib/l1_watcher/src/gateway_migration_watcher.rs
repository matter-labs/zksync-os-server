use std::marker::PhantomData;

use tokio::sync::mpsc;
use zksync_os_contract_interface::ServerNotifier::MigrateFromGateway;

use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event};
use alloy::primitives::{Address, ChainId};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;
use zksync_os_contract_interface::{ServerNotifier::MigrateToGateway, ZkChain};
use zksync_os_types::SystemTxEnvelope;

pub trait MigrationProcessor: Send + Sync + 'static {
    type Event: SolEvent + Send + Sync + 'static;

    fn chain_id(event: Self::Event) -> ChainId;
}

pub struct Gateway;

pub struct L1;

impl MigrationProcessor for Gateway {
    type Event = MigrateFromGateway;

    fn chain_id(event: Self::Event) -> ChainId {
        event.chainId.try_into().unwrap()
    }
}

impl MigrationProcessor for L1 {
    type Event = MigrateToGateway;

    fn chain_id(event: Self::Event) -> ChainId {
        event.chainId.try_into().unwrap()
    }
}

pub struct GatewayMigrationWatcher<T> {
    server_notifier_contract: Address,
    output: mpsc::Sender<SystemTxEnvelope>,

    _marker: PhantomData<T>,
}

impl<T: MigrationProcessor> GatewayMigrationWatcher<T> {
    pub async fn create_watcher(
        zk_chain: ZkChain<DynProvider>,
        config: L1WatcherConfig,
        output: mpsc::Sender<SystemTxEnvelope>,
    ) -> anyhow::Result<L1Watcher> {
        let this = Self {
            server_notifier_contract: zk_chain.get_server_notifier_address().await?,
            output,
            _marker: PhantomData,
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
impl<T: MigrationProcessor> ProcessL1Event for GatewayMigrationWatcher<T> {
    const NAME: &'static str = "gateway_migration";

    type SolEvent = T::Event;
    type WatchedEvent = T::Event;

    fn contract_address(&self) -> Address {
        self.server_notifier_contract
    }

    async fn process_event(&mut self, tx: T::Event, _log: Log) -> Result<(), L1WatcherError> {
        let envelope = SystemTxEnvelope::set_sl_chain_id(T::chain_id(tx));

        self.output
            .send(envelope)
            .await
            .map_err(|_| L1WatcherError::OutputClosed)
    }
}
