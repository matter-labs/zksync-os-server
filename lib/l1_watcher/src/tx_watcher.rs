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
            config.finalized_ingestion,
            zk_chain_address_l1 = ?zk_chain_l1.address(),
            zk_chain_address_sl = ?zk_chain_sl.address(),
            "initializing L1 transaction watcher"
        );

        let provider = zk_chain_l1.provider().clone();
        let address = (*zk_chain_l1.address()).into();
        let l1_chain_id = provider.get_chain_id().await?;
        let max_blocks_to_process = config.max_blocks_to_process;

        let resolve_start = move |next_l1_priority_id: u64| async move {
            let next_l1_block = find_l1_block_by_priority_id(
                &zk_chain_l1,
                next_l1_priority_id,
                max_blocks_to_process,
            )
            .await?;
            tracing::info!(next_l1_block, "resolved on L1");
            let processor = Self {
                next_l1_priority_id,
                zk_chain_sl,
                cached_total_priority_ops_resp: None,
                sink: Box::new(sink),
            };
            Ok((next_l1_block, processor))
        };

        if config.finalized_ingestion {
            Ok(StartResolver::new_finalized(
                config,
                provider,
                address,
                None,
                resolve_start,
            ))
        } else {
            StartResolver::new(config, provider, address, None, l1_chain_id, resolve_start).await
        }
    }
}

/// The L1 block to scan `NewPriorityRequest` events forward from: the block of the
/// newest event with `txId < next_l1_priority_id` (priority ids are sequential, so
/// everything at or after that block covers the ids still to process; already-seen
/// ids are skipped by `process_event`). Resolved by scanning logs backward from the
/// head — no historical *state* queries, which fail on RPCs with bounded state
/// retention once the chain has aged.
async fn find_l1_block_by_priority_id(
    zk_chain: &ZkChain<NodeProvider>,
    next_l1_priority_id: u64,
    max_blocks_per_query: u64,
) -> anyhow::Result<BlockNumber> {
    use alloy::sol_types::SolEvent as _;
    if next_l1_priority_id == 0 {
        // Nothing processed yet: scan from the chain's beginning.
        return zk_chain.deployment_block().await;
    }
    let provider = zk_chain.provider();
    let latest = provider.get_block_number().await?;
    for (from, to) in util::backward_windows(latest, max_blocks_per_query) {
        let logs = provider
            .get_logs(
                &alloy::rpc::types::Filter::new()
                    .address(*zk_chain.address())
                    .event_signature(NewPriorityRequest::SIGNATURE_HASH)
                    .from_block(from)
                    .to_block(to),
            )
            .await?;
        for log in logs.iter().rev() {
            let tx_id = NewPriorityRequest::decode_log(&log.inner)?.txId;
            if tx_id < alloy::primitives::U256::from(next_l1_priority_id) {
                return Ok(log
                    .block_number
                    .expect("indexed event log without block number"));
            }
        }
    }
    // No earlier request found although some were processed — the chain data is
    // gone or the cursor is wrong; scanning from the beginning stays correct.
    zk_chain.deployment_block().await
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
