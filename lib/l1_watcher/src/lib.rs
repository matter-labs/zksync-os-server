mod config;


pub use crate::config::L1WatcherConfig;
use zksync_os_types::L1Envelope;

use alloy::consensus::Transaction;
use alloy::eips::BlockId;
use alloy::network::Ethereum;
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::{DynProvider, Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Context;
use tokio::sync::mpsc;
use std::time::Duration;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_contract_interface::{Bridgehub, ZkChain};

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
        // The first priority transaction to be retrieved here is the earliest one that wasn't
        // `executed` on-chain yet. The main sequencer stream starts earlier than that, but we will
        // not have `Produce` commands before this number. We don't validate priority transactions
        // yet, so it's fine
        let next_l1_block = find_first_unprocessed_l1_block(zk_chain).await?;
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

async fn find_first_unprocessed_l1_block(
    zk_chain: ZkChain<DynProvider>,
) -> anyhow::Result<BlockNumber> {
    let block_number = zk_chain.provider().get_block_number().await?;
    let next_l1_priority_id = zk_chain
        .get_first_unprocessed_priority_tx_at_block(block_number.into())
        .await?;
    // We want to find the first block where `total_l1_priority_txs(block) >= next_l1_priority_id`
    // (not strict equality as multiple L1->L2 transactions can be added in a single L1 block).
    // Invariant is to maintain interval `[low; high)` where:
    // * `total_l1_priority_txs(low) < next_l1_priority_id`
    // * `total_l1_priority_txs(high) >= next_l1_priority_id`
    let mut low = 0;
    let mut high = block_number + 1;
    while (high - low) > 1 {
        let mid = (low + high) / 2;
        // Assume 0 total L1 transactions when the call fails (presume that ZK chain has not
        // been deployed yet).
        let mid_l1_priority_id = zk_chain
            .get_total_priority_txs_at_block(mid.into())
            .await
            .unwrap_or(0);

        if mid_l1_priority_id < next_l1_priority_id {
            low = mid;
        } else {
            high = mid;
        }
    }
    // Resulting range is `[low; low + 1)` meaning next L1 priority transaction was observed in
    // block `low`
    Ok(low)
}
