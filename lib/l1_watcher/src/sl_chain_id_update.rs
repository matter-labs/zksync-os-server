use crate::ProcessL1Event;
use crate::watcher::{L1Watcher, L1WatcherError};
use alloy::primitives::Address;
use alloy::rpc::types::Log;
use zksync_os_contract_interface::SettlementLayerChainIdUpdated;

pub struct SLChainIdUpdateWatcher {}

impl SLChainIdUpdateWatcher {
    pub async fn create_watcher() -> anyhow::Result<L1Watcher> {
        todo!()
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
        log: Log,
    ) -> Result<(), L1WatcherError> {
        todo!()
    }
}
