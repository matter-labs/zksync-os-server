mod config;
mod rocksdb;

pub use crate::config::L1WatcherConfig;
use zksync_os_mempool::DynL1Pool;
use zksync_os_types::L1Envelope;

use crate::rocksdb::L1WatcherRocksdbStorage;
use alloy::consensus::Transaction;
use alloy::eips::BlockId;
use alloy::network::Ethereum;
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::{DynProvider, Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Context;
use std::time::Duration;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;

pub struct L1Watcher {
    provider: DynProvider<Ethereum>,
    l1_pool: DynL1Pool,
    zk_chain_address: Address,
    poll_interval: Duration,
    max_blocks_to_process: u64,
    storage: L1WatcherRocksdbStorage,
}

impl L1Watcher {
    pub async fn new(
        config: L1WatcherConfig,
        l1_pool: DynL1Pool,
        chain_id: u64,
    ) -> anyhow::Result<Self> {
        let storage = L1WatcherRocksdbStorage::new(config.rocks_db_path);

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
            next_l1_block = storage.next_l1_block().unwrap_or(0),
            "initializing L1 watcher"
        );
        let bridgehub = Bridgehub::new(
            config.bridgehub_address.0.into(),
            provider.clone(),
            chain_id,
        );
        let zk_chain_address = bridgehub.zk_chain_address().await?;
        tracing::info!(?zk_chain_address, "resolved on L1");
        Ok(Self {
            provider,
            l1_pool,
            zk_chain_address,
            poll_interval: config.poll_interval,
            max_blocks_to_process: config.max_blocks_to_process,
            storage,
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
        // TODO: Do not start from genesis; figure out when diamond proxy was first deployed instead?
        //       Alternatively presume that we should continue from `latest_block - N` and panic if
        //       first priority id does not match expected value.
        let from_block = self.storage.next_l1_block().unwrap_or(0);
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
            // We assume mempool is persistent, i.e. that inserting an L1 transaction into it
            // guarantees it will get executed eventually.
            //
            // Moreover, we assume that all transactions are idempotent - inserting the same
            // transaction multiple times does not affect sequencer's operation where sequencer is
            // the sole consumer of mempool.
            self.l1_pool.add_transaction(tx);
        }
        // L1 transactions already added to mempool are guaranteed to be processed eventually so we
        // do not have to process these blocks ever again. If L1 watcher were to fail before calling
        // `set_next_l1_block` then these L1 transactions will be added to mempool twice but that
        // does not matter as we assume sequencer will treat them  as idempotent (see comment above).
        self.storage.set_next_l1_block(to_block + 1);
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
