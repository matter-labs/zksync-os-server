use tokio::sync::mpsc;

use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event};
use alloy::primitives::Address;
use alloy::providers::DynProvider;
use alloy::rpc::types::Log;
use zksync_os_contract_interface::{SettlementLayerChainIdUpdated, ZkChain};
use zksync_os_types::{SystemTxEnvelope, SystemTxInput};

pub struct SLChainIdUpdateWatcher {
    output: mpsc::Sender<SystemTxEnvelope>,
}

impl SLChainIdUpdateWatcher {
    pub async fn create_watcher(
        zk_chain: ZkChain<DynProvider>,
        config: L1WatcherConfig,
        output: mpsc::Sender<SystemTxEnvelope>,
    ) -> anyhow::Result<L1Watcher> {
        let this = Self { output };
        let next_l1_block = 0; // TODO: implement this
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
impl ProcessL1Event for SLChainIdUpdateWatcher {
    const NAME: &'static str = "sl_chain_id_update";

    type SolEvent = SettlementLayerChainIdUpdated;
    type WatchedEvent = SettlementLayerChainIdUpdated;

    fn contract_address(&self) -> Address {
        todo!()
    }

    async fn process_event(
        &mut self,
        tx: SettlementLayerChainIdUpdated,
        _log: Log,
    ) -> Result<(), L1WatcherError> {
        let envelope = SystemTxEnvelope::new(SystemTxInput::SetSLChainId(
            tx._newSettlementLayerChainId.try_into().unwrap(),
        ));

        self.output
            .send(envelope)
            .await
            .map_err(|_| L1WatcherError::OutputClosed)
    }
}
