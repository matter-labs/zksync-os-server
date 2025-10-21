use crate::watcher::{L1Watcher, L1WatcherError, ProcessL1Event};
use crate::{L1WatcherConfig, util};
use alloy::primitives::BlockNumber;
use alloy::providers::{DynProvider, Provider};
use std::convert::Infallible;
use std::sync::Arc;
use zksync_os_contract_interface::IExecutor::BlockExecution;
use zksync_os_contract_interface::ZkChain;
use zksync_os_storage_api::{ReadBatch, WriteFinality};

/// Don't try to process that many block linearly
const MAX_L1_BLOCKS_LOOKBEHIND: u64 = 100_000;

pub struct L1ExecuteWatcher<Finality, BatchStorage> {
    next_batch_number: u64,
    finality: Finality,
    batch_storage: BatchStorage,
}

impl<Finality: WriteFinality, BatchStorage: ReadBatch> L1ExecuteWatcher<Finality, BatchStorage> {
    pub async fn new(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        finality: Finality,
        batch_storage: BatchStorage,
    ) -> anyhow::Result<L1Watcher<Self>> {
        let current_l1_block = zk_chain.provider().get_block_number().await?;
        let last_executed_batch = finality.get_finality_status().last_executed_batch;
        tracing::info!(
            current_l1_block,
            last_executed_batch,
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address = ?zk_chain.address(),
            "initializing L1 execute watcher"
        );
        let last_l1_block = find_l1_execute_block_by_batch_number(zk_chain.clone(), last_executed_batch)
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
        tracing::info!(last_l1_block, "resolved on L1");

        let this = Self {
            next_batch_number: last_executed_batch + 1,
            finality,
            batch_storage,
        };
        let l1_watcher = L1Watcher::new(
            zk_chain,
            // We start from last L1 block as it may contain more executed batches apart from the last
            // one.
            last_l1_block,
            config.max_blocks_to_process,
            config.poll_interval,
            this,
        );

        Ok(l1_watcher)
    }
}

async fn find_l1_execute_block_by_batch_number(
    zk_chain: ZkChain<DynProvider>,
    batch_number: u64,
) -> anyhow::Result<BlockNumber> {
    util::find_l1_block_by_predicate(Arc::new(zk_chain), move |zk, block| async move {
        let res = zk.get_total_batches_executed(block.into()).await?;
        Ok(res >= batch_number)
    })
    .await
}

impl<Finality: WriteFinality, BatchStorage: ReadBatch> ProcessL1Event
    for L1ExecuteWatcher<Finality, BatchStorage>
{
    const NAME: &'static str = "block_execution";

    type SolEvent = BlockExecution;
    type WatchedEvent = BlockExecution;
    type Error = Infallible;

    async fn process_event(
        &mut self,
        batch_execute: BlockExecution,
    ) -> Result<(), L1WatcherError<Self::Error>> {
        let batch_number = batch_execute.batchNumber.to::<u64>();
        let batch_hash = batch_execute.batchHash;
        let batch_commitment = batch_execute.commitment;
        if batch_number < self.next_batch_number {
            tracing::debug!(
                batch_number,
                ?batch_hash,
                ?batch_commitment,
                "skipping already processed executed batch",
            );
        } else {
            let (_, last_executed_block) = self
                .batch_storage
                .get_batch_range_by_number(batch_number)
                .await?
                .expect("executed batch is missing");
            self.finality.update_finality_status(|finality| {
                assert!(
                    batch_number > finality.last_executed_batch,
                    "non-monotonous executed batch"
                );
                assert!(
                    last_executed_block > finality.last_executed_block,
                    "non-monotonous executed block"
                );
                finality.last_executed_batch = batch_number;
                finality.last_executed_block = last_executed_block;
            });
            tracing::debug!(
                batch_number,
                ?batch_hash,
                ?batch_commitment,
                last_executed_block,
                "discovered executed batch"
            );
        }
        Ok(())
    }
}
