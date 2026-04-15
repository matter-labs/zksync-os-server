use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event, util};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Log;
use std::sync::Arc;
use std::time::Duration;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_contract_interface::ZkChain;
use zksync_os_mempool::subpools::l1::L1Subpool;
use zksync_os_types::L1PriorityEnvelope;

/// Watches L1 priority transaction events and feeds them into the L1 transaction subpool.
///
/// This component reads `NewPriorityRequest` events from the L1 mailbox, waits until the same
/// priority request is visible from the settlement layer, and then inserts the corresponding
/// `L1PriorityEnvelope` into `L1Subpool`.
pub struct L1TxWatcher {
    contract_address: Address,
    next_l1_priority_id: u64,
    zk_chain_sl: ZkChain<DynProvider>,
    cached_total_priority_ops_resp: Option<u64>,
    l1_subpool: L1Subpool,
}

impl L1TxWatcher {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain_l1: ZkChain<DynProvider>,
        zk_chain_sl: ZkChain<DynProvider>,
        l1_subpool: L1Subpool,
        next_l1_priority_id: u64,
    ) -> anyhow::Result<L1Watcher> {
        tracing::info!(
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address_l1 = ?zk_chain_l1.address(),
            zk_chain_address_sl = ?zk_chain_sl.address(),
            "initializing L1 transaction watcher"
        );

        let next_l1_block =
            find_l1_block_by_priority_id(zk_chain_l1.clone(), next_l1_priority_id).await?;

        tracing::info!(next_l1_block, "resolved on L1");

        let this = Self {
            contract_address: *zk_chain_l1.address(),
            next_l1_priority_id,
            zk_chain_sl,
            cached_total_priority_ops_resp: None,
            l1_subpool,
        };
        let l1_watcher = L1Watcher::new(
            zk_chain_l1.provider().clone(),
            next_l1_block,
            config.max_blocks_to_process,
            config.confirmations,
            zk_chain_l1.provider().get_chain_id().await?,
            config.poll_interval,
            this.into(),
        )
        .await?;

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
            if let Some(total_priority_ops) = self.cached_total_priority_ops_resp
                && total_priority_ops > tx.priority_id()
            {
                // tx is processed on SL, we can proceed with inserting it to subpool
            } else {
                tracing::debug!(
                    priority_id = tx.priority_id(),
                    hash = ?tx.hash(),
                    "waiting for tx to be processed on SL"
                );
                let mut timer = tokio::time::interval(Duration::from_secs(10));
                loop {
                    timer.tick().await;
                    let total_priority_ops = self
                        .zk_chain_sl
                        .get_total_priority_txs_at_block(BlockId::Number(BlockNumberOrTag::Latest))
                        .await?;
                    self.cached_total_priority_ops_resp = Some(total_priority_ops);
                    if total_priority_ops > tx.priority_id() {
                        break;
                    }
                }
            };
            self.next_l1_priority_id = tx.priority_id() + 1;
            tracing::debug!(
                priority_id = tx.priority_id(),
                hash = ?tx.hash(),
                "sending new priority transaction for processing",
            );
            self.l1_subpool.insert(Arc::new(tx)).await;
        }
        Ok(())
    }
}
