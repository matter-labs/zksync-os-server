use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event, util};
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::DynProvider;
use alloy::rpc::types::Log;
use std::sync::Arc;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_contract_interface::ZkChain;
use zksync_os_mempool::subpools::l1::L1Subpool;
use zksync_os_types::L1PriorityEnvelope;

pub struct L1TxWatcher {
    contract_address: Address,
    next_l1_priority_id: u64,
    l1_subpool: L1Subpool,
}

impl L1TxWatcher {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        l1_subpool: L1Subpool,
        next_l1_priority_id: u64,
    ) -> anyhow::Result<L1Watcher> {
        tracing::info!(
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address = ?zk_chain.address(),
            "initializing L1 transaction watcher"
        );

        let next_l1_block =
            find_l1_block_by_priority_id(zk_chain.clone(), next_l1_priority_id).await?;

        tracing::info!(next_l1_block, "resolved on L1");

        let this = Self {
            contract_address: *zk_chain.address(),
            next_l1_priority_id,
            l1_subpool,
        };
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

async fn find_l1_block_by_priority_id(
    zk_chain: ZkChain<DynProvider>,
    next_l1_priority_id: u64,
) -> anyhow::Result<BlockNumber> {
    util::find_l1_block_by_predicate(Arc::new(zk_chain), 0, move |zk, block| async move {
        let res = zk.get_total_priority_txs_at_block(block.into()).await?;
        Ok(res >= next_l1_priority_id)
    })
    .await
}

#[async_trait::async_trait]
impl ProcessL1Event for L1TxWatcher {
    const NAME: &'static str = "priority_tx";

    type SolEvent = NewPriorityRequest;
    type WatchedEvent = L1PriorityEnvelope;

    fn contract_address(&self) -> Address {
        self.contract_address
    }

    async fn process_event(
        &mut self,
        tx: L1PriorityEnvelope,
        _log: Log,
    ) -> Result<(), L1WatcherError> {
        if tx.priority_id() < self.next_l1_priority_id {
            tracing::debug!(
                priority_id = tx.priority_id(),
                hash = ?tx.hash(),
                "skipping already processed priority transaction",
            );
        } else {
            self.next_l1_priority_id = tx.priority_id() + 1;
            tracing::debug!(
                priority_id = tx.priority_id(),
                hash = ?tx.hash(),
                "sending new priority transaction for processing",
            );
            self.l1_subpool.insert(Arc::new(tx));
        }
        Ok(())
    }
}
