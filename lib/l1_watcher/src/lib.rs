mod config;
mod metrics;

pub use crate::config::L1WatcherConfig;
pub use crate::metrics::L1_METRICS;

use alloy::consensus::Transaction;
use alloy::eips::BlockId;
use alloy::network::Ethereum;
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::{DynProvider, Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Context;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_contract_interface::{Bridgehub, ZkChain};
use zksync_os_types::L1Envelope;

/// Don't try to process that many block linearly
const MAX_L1_BLOCKS_LOOKBEHIND: u64 = 100_000;

pub struct L1Watcher {
    provider: DynProvider<Ethereum>,
    zk_chain_address: Address,
    next_l1_block: BlockNumber,

    output: mpsc::Sender<L1Envelope>,

    poll_interval: Duration,
    max_blocks_to_process: u64,
}

impl L1Watcher {
    pub async fn new(
        config: L1WatcherConfig,
        chain_id: u64,
        output: mpsc::Sender<L1Envelope>,
        next_l1_priority_id: u64,
    ) -> anyhow::Result<Self> {
        let provider = DynProvider::new(
            ProviderBuilder::new()
                .connect_ws(WsConnect::new(config.l1_api_url))
                .await
                .context("failed to connect to L1 api")?,
        );
        tracing::info!(
            chain_id,
            config.max_blocks_to_process,
            ?config.poll_interval,
            ?config.bridgehub_address,
            "initializing L1 watcher"
        );
        let bridgehub = Bridgehub::new(
            config.bridgehub_address.0.into(),
            provider.clone(),
            chain_id,
        );
        let zk_chain = bridgehub.zk_chain().await?;
        let zk_chain_address = *zk_chain.address();

        let current_l1_block = provider.get_block_number().await?;
        let next_l1_block = find_l1_block_by_priority_id(&zk_chain, next_l1_priority_id)
            .await
            .or_else(|err| {
                // This may error on Anvil with `--load-state` - as it doesn't support `eth_call` even for recent blocks.
                // We default to `0` in this case - `eth_getLogs` are still supported.
                // Assert that we don't fallback on longer chains (e.g. Sepolia)
                if current_l1_block > MAX_L1_BLOCKS_LOOKBEHIND {
                    anyhow::bail!(
                        "Binary search failed with {}. Cannot default starting block to zero for a long chain. Current L1 block number: {}. Limit: {}.",
                        err,
                        current_l1_block,
                        MAX_L1_BLOCKS_LOOKBEHIND,
                    )
                } else {
                    Ok(0)
                }
            })?;

        tracing::info!(?zk_chain_address, next_l1_block, "resolved on L1");

        Ok(Self {
            provider,
            zk_chain_address,
            next_l1_block,
            output,
            poll_interval: config.poll_interval,
            max_blocks_to_process: config.max_blocks_to_process,
        })
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut timer = tokio::time::interval(self.poll_interval);
        loop {
            timer.tick().await;
            self.poll().await?;
        }
    }
}

impl L1Watcher {
    /// Processes up to `self.max_blocks_to_process` new L1 blocks for priority requests and adds
    /// them to mempool as L1 transactions.
    async fn poll(&mut self) -> anyhow::Result<()> {
        let latest_block = self
            .provider
            .get_block(BlockId::latest())
            .await?
            .context("L1 does not have any blocks")?;
        let from_block = self.next_l1_block;
        // Inspect up to `self.max_blocks_to_process` blocks at a time
        let to_block = latest_block
            .header
            .number
            .min(from_block + self.max_blocks_to_process - 1);
        if from_block > to_block {
            return Ok(());
        }
        let priority_txs = self.process_l1_blocks(from_block, to_block).await?;
        L1_METRICS
            .l1_transactions_loaded
            .inc_by(priority_txs.len() as u64);
        L1_METRICS.most_recently_scanned_l1_block.set(to_block);

        for tx in priority_txs {
            tracing::debug!(
                serial_id = tx.nonce(),
                hash = ?tx.hash(),
                "adding new priority transaction to mempool",
            );
            self.output.send(tx).await?;
        }

        self.next_l1_block = to_block + 1;
        Ok(())
    }

    /// Processes a range of L1 blocks for new priority requests.
    ///
    /// Returns a list of priority transactions extracted from the L1 blocks.
    async fn process_l1_blocks(
        &self,
        from: BlockNumber,
        to: BlockNumber,
    ) -> anyhow::Result<Vec<L1Envelope>> {
        let filter = Filter::new()
            .from_block(from)
            .to_block(to)
            .event_signature(NewPriorityRequest::SIGNATURE_HASH)
            .address(self.zk_chain_address);
        let priority_logs = self.provider.get_logs(&filter).await?;
        let priority_txs = priority_logs
            .into_iter()
            .map(|log| {
                let priority_request = NewPriorityRequest::decode_log(&log.inner)?;
                anyhow::Ok(L1Envelope::try_from(priority_request.data.transaction)?)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        if priority_txs.is_empty() {
            tracing::trace!("no new priority txs");
        } else {
            // unwraps are safe because the vec is not empty
            let first = priority_txs.first().unwrap();
            let last = priority_txs.last().unwrap();
            tracing::info!(
                first_serial_id = %first.nonce(),
                last_serial_id = %last.nonce(),
                "received priority transactions",
            );
        }

        Ok(priority_txs)
    }
}

async fn find_l1_block_by_priority_id(
    zk_chain: &ZkChain<DynProvider>,
    next_l1_priority_id: u64,
) -> anyhow::Result<BlockNumber> {
    let latest = zk_chain.provider().get_block_number().await?;

    async fn predicate(zk: &ZkChain<DynProvider>, block: u64, target: u64) -> anyhow::Result<bool> {
        if !zk.code_exists_at_block(block.into()).await? {
            // return early if contract is not deployed yet - otherwise `get_total_priority_txs_at_block` will fail
            return Ok(false);
        }
        let res = zk.get_total_priority_txs_at_block(block.into()).await?;
        Ok(res >= target)
    }

    // Ensure the predicate is true by the upper bound, or bail early.
    if !predicate(zk_chain, latest, next_l1_priority_id).await? {
        anyhow::bail!(
            "Condition not satisfied up to latest block: contract not deployed yet \
             or target {} not reached.",
            next_l1_priority_id
        );
    }

    // Binary search on [0, latest] for the first block where predicate is true.
    let (mut lo, mut hi) = (0u64, latest);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if predicate(zk_chain, mid, next_l1_priority_id).await? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }

    Ok(lo)
}
