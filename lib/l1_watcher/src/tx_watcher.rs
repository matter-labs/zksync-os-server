use crate::watcher::{L1WatcherError, StartResolver};
use crate::{EventSink, L1WatcherConfig, ProcessL1Event, util};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::BlockNumber;
use alloy::providers::Provider;
use alloy::rpc::types::Log;
use std::sync::Arc;
use std::time::Duration;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_contract_interface::ZkChain;
use zksync_os_provider::NodeProvider;
use zksync_os_types::L1PriorityEnvelope;

/// Watches L1 priority transaction events and feeds them into the L1 transaction subpool.
///
/// This component reads `NewPriorityRequest` events from the L1 mailbox, waits until the same
/// priority request is visible from the settlement layer, and then inserts the corresponding
/// `L1PriorityEnvelope` into its sink.
pub struct L1TxWatcher {
    next_l1_priority_id: u64,
    zk_chain_sl: ZkChain<NodeProvider>,
    cached_total_priority_ops_resp: Option<u64>,
    sink: Box<dyn EventSink<Arc<L1PriorityEnvelope>>>,
}

impl L1TxWatcher {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain_l1: ZkChain<NodeProvider>,
        zk_chain_sl: ZkChain<NodeProvider>,
        sink: impl EventSink<Arc<L1PriorityEnvelope>>,
    ) -> anyhow::Result<StartResolver<u64, Self>> {
        tracing::info!(
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address_l1 = ?zk_chain_l1.address(),
            zk_chain_address_sl = ?zk_chain_sl.address(),
            "initializing L1 transaction watcher"
        );

        let provider = zk_chain_l1.provider().clone();
        let address = (*zk_chain_l1.address()).into();
        let l1_chain_id = provider.get_chain_id().await?;

        let resolve_start = move |next_l1_priority_id: u64| async move {
            let next_l1_block =
                find_l1_block_by_priority_id(zk_chain_l1.clone(), next_l1_priority_id).await?;
            tracing::info!(next_l1_block, "resolved on L1");
            let processor = Self {
                next_l1_priority_id,
                zk_chain_sl,
                cached_total_priority_ops_resp: None,
                sink: Box::new(sink),
            };
            Ok((next_l1_block, processor))
        };

        StartResolver::new(config, provider, address, None, l1_chain_id, resolve_start).await
    }
}

async fn find_l1_block_by_priority_id(
    zk_chain: ZkChain<NodeProvider>,
    next_l1_priority_id: u64,
) -> anyhow::Result<BlockNumber> {
    let deployment_block = zk_chain.deployment_block().await?;
    util::find_l1_block_by_predicate(
        Arc::new(zk_chain),
        deployment_block,
        move |zk, block| async move {
            let res = zk.get_total_priority_txs_at_block(block.into()).await?;
            Ok(res >= next_l1_priority_id)
        },
    )
    .await
}

#[async_trait::async_trait]
impl ProcessL1Event for L1TxWatcher {
    const NAME: &'static str = "priority_tx";

    type SolEvent = NewPriorityRequest;
    type WatchedEvent = L1PriorityEnvelope;

    async fn process_event(
        &mut self,
        _provider: &NodeProvider,
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
            self.sink.push(Arc::new(tx)).await;
        }
        Ok(())
    }
}
