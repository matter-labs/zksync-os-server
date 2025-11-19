use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event, util};
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::{DynProvider, Provider};
use std::sync::Arc;
use tokio::sync::mpsc;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_contract_interface::ZkChain;
use zksync_os_types::L1PriorityEnvelope;

/// Don't try to process that many block linearly
const MAX_L1_BLOCKS_LOOKBEHIND: u64 = 100_000;

pub struct L1TxWatcher {
    contract_address: Address,
    next_l1_priority_id: u64,
    output: mpsc::Sender<L1PriorityEnvelope>,
}

impl L1TxWatcher {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        output: mpsc::Sender<L1PriorityEnvelope>,
        next_l1_priority_id: u64,
    ) -> anyhow::Result<L1Watcher> {
        tracing::info!(
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address = ?zk_chain.address(),
            "initializing L1 transaction watcher"
        );

        let current_l1_block = zk_chain.provider().get_block_number().await?;
        let next_l1_block = find_l1_block_by_priority_id(zk_chain.clone(), next_l1_priority_id)
            .await
            .or_else(|err| {
                // This may error on Anvil with `--load-state` - as it doesn't support `eth_call` even for recent blocks.
                // We default to `0` in this case - `eth_getLogs` are still supported.
                // Assert that we don't fallback on longer chains (e.g. Sepolia)
                if current_l1_block > MAX_L1_BLOCKS_LOOKBEHIND {
                    anyhow::bail!(
                        "Binary search failed with {err}. Cannot default starting block to zero for a long chain. Current L1 block number: {current_l1_block}. Limit: {MAX_L1_BLOCKS_LOOKBEHIND}."
                    )
                } else {
                    Ok(0)
                }
            })?;

        tracing::info!(next_l1_block, "resolved on L1");

        let this = Self {
            contract_address: *zk_chain.address(),
            next_l1_priority_id,
            output,
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
    util::find_l1_block_by_predicate(Arc::new(zk_chain), move |zk, block| async move {
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

    async fn process_event(&mut self, tx: L1PriorityEnvelope) -> Result<(), L1WatcherError> {
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
            self.output
                .send(tx)
                .await
                .map_err(|_| L1WatcherError::OutputClosed)?;
        }
        Ok(())
    }
}
