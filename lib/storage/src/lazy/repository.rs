use crate::db::RepositoryDb;
use crate::in_memory::RepositoryInMemory;
use crate::metrics::REPOSITORIES_METRICS;
use alloy::primitives::B256;
use alloy::primitives::{Address, BlockHash, BlockNumber, TxHash, TxNonce};
use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::ops::{Div, Range};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use zksync_os_genesis::Genesis;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_storage_api::notifications::{BlockNotification, SubscribeToBlocks};
use zksync_os_storage_api::{
    LogIndex, ReadRepository, RepositoryBlock, RepositoryResult, StoredTxData, TxMeta,
    WriteRepository,
};
use zksync_os_types::{BlockOutput, ZkReceiptEnvelope, ZkTransaction};

/// Size of the broadcast channel used to notify about new blocks.
const BLOCK_NOTIFICATION_CHANNEL_SIZE: usize = 256;

/// Manages a composed view on in-memory repositories and DB-backed repositories.
/// Persists in-memory objects in the background and makes sure in-memory storage does not grow above
/// `max_blocks_in_memory`.
#[derive(Clone, Debug)]
pub struct RepositoryManager {
    in_memory: RepositoryInMemory,
    db: RepositoryDb,
    max_blocks_in_memory: u64,
    block_sender: broadcast::Sender<BlockNotification>,
    db_ready_to_process_blocks: Arc<AtomicBool>,
    /// Highest block PRUNED from the in-memory window by the persist loop. `populate` gates on
    /// this (not on DB persistence, which now runs far ahead): memory is hard-bounded at
    /// `max_blocks_in_memory` blocks even if pruning lags production.
    pruned_block_number: watch::Sender<u64>,
}

impl RepositoryManager {
    pub async fn new(blocks_to_retain: usize, db_path: PathBuf, genesis: &Genesis) -> Self {
        let db = RepositoryDb::new(&db_path, genesis).await;
        let genesis_block = db
            .get_block_by_number(0)
            .unwrap()
            .expect("Missing genesis block in DB");
        let (block_sender, _) = broadcast::channel(BLOCK_NOTIFICATION_CHANNEL_SIZE);

        RepositoryManager {
            // Initializes in-memory repository with genesis block. It is never pruned from cache.
            in_memory: RepositoryInMemory::new(genesis_block),
            db,
            max_blocks_in_memory: blocks_to_retain as u64,
            block_sender,
            db_ready_to_process_blocks: Arc::new(AtomicBool::new(false)),
            pruned_block_number: watch::channel(0).0,
        }
    }

    // fixme: as this loop is not tied to state compacting, it can fall behind and result in
    //        unrecoverable state on restart
    pub async fn run_persist_loop(self) {
        loop {
            if self.db_ready_to_process_blocks.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Drain up to `PERSIST_GROUP` available blocks per iteration into batched RocksDB
        // writes: the per-block commit capped persistence at ~190 blocks/s (measured at
        // ~1,850-tx blocks), and once the in-memory retention window fills, `populate` waits on
        // persistence — making this loop the whole pipeline's throughput ceiling on long runs.
        const PERSIST_GROUP: u64 = 128;
        // Pruning is DECOUPLED from persisting: the blob-persisted DB (bench branch) does not
        // serve tx-by-hash reads, so the most recent `max_blocks_in_memory` blocks stay in RAM
        // to serve receipts even after they are durably persisted. Memory is bounded by that
        // window; persistence runs eagerly ahead of it.
        let mut next_to_prune: BlockNumber = 1;
        loop {
            let db_block_number = self.db.get_latest_block();
            self.in_memory
                .wait_for_block_number(db_block_number + 1)
                .await;

            let last_block_number = self
                .in_memory
                .get_latest_block()
                .min(db_block_number + PERSIST_GROUP);
            // Headers only: persistence (headers-only, see `write_blocks`) and pruning (the
            // tx-hash list is inside the block body) never need the tx data — fetching it here
            // was ~1.6M wasted per-tx map lookups + Arc clone/drops per second at benchmark
            // rates, racing `populate`'s inserts on the same shards.
            let group: Vec<_> = (db_block_number + 1..=last_block_number)
                .map(|block_number| {
                    self.in_memory
                        .get_block_by_number(block_number)
                        .expect("repository read failed")
                        .expect("missing in-memory block")
                })
                .collect();
            let tx_count: usize = group.iter().map(|b| b.body.transactions.len()).sum();

            let persist_latency_observer = REPOSITORIES_METRICS.persist_block.start();
            self.db.write_blocks(&group);
            let persist_latency = persist_latency_observer.observe();
            REPOSITORIES_METRICS
                .persist_block_per_tx
                .observe(persist_latency.div(tx_count.max(1) as u32));

            // Prune persisted blocks that have fallen out of the in-memory serving window
            // (parallel: per-block removal is another ~1,800 per-tx map operations).
            let prune_up_to = self
                .in_memory
                .get_latest_block()
                .saturating_sub(self.max_blocks_in_memory)
                .min(last_block_number);
            if next_to_prune <= prune_up_to {
                (next_to_prune..=prune_up_to).into_par_iter().for_each(
                    |block_number| {
                        if let Ok(Some(block)) = self.in_memory.get_block_by_number(block_number)
                        {
                            self.in_memory.remove_block_and_transactions(
                                block.number,
                                &block.body.transactions,
                            );
                        }
                    },
                );
                next_to_prune = prune_up_to + 1;
            }
            self.pruned_block_number.send_replace(next_to_prune - 1);

            let persistence_lag = self
                .in_memory
                .get_latest_block()
                .saturating_sub(last_block_number) as usize;
            REPOSITORIES_METRICS.persistence_lag.set(persistence_lag);
            tracing::info!(
                blocks = group.len(),
                block_number = last_block_number,
                ?persist_latency,
                persistence_lag,
                "persisted blocks",
            );

            REPOSITORIES_METRICS.persist_block_number.set(last_block_number);
        }
    }

    pub async fn wait_for_db_ready_to_process_blocks(&self) {
        while !self.db_ready_to_process_blocks.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(1)).await;
            tracing::debug!("waiting for `db_ready_to_process_blocks`");
        }
    }
}

impl LogIndex for RepositoryManager {
    fn blocks_for_address(
        &self,
        address: Address,
        range: Range<u64>,
    ) -> RepositoryResult<(RoaringBitmap, Range<u64>)> {
        self.db.blocks_for_address(address, range)
    }

    fn blocks_for_topic(
        &self,
        topic: B256,
        range: Range<u64>,
    ) -> RepositoryResult<(RoaringBitmap, Range<u64>)> {
        self.db.blocks_for_topic(topic, range)
    }
}

impl ReadRepository for RepositoryManager {
    fn get_block_by_number(
        &self,
        number: BlockNumber,
    ) -> RepositoryResult<Option<RepositoryBlock>> {
        if let Some(block) = self.in_memory.get_block_by_number(number)? {
            return Ok(Some(block));
        }

        self.db.get_block_by_number(number)
    }

    fn get_block_by_hash(&self, hash: BlockHash) -> RepositoryResult<Option<RepositoryBlock>> {
        if let Some(block) = self.in_memory.get_block_by_hash(hash)? {
            return Ok(Some(block));
        }

        self.db.get_block_by_hash(hash)
    }

    fn get_raw_transaction(&self, hash: TxHash) -> RepositoryResult<Option<Vec<u8>>> {
        if let Some(raw_tx) = self.in_memory.get_raw_transaction(hash)? {
            return Ok(Some(raw_tx));
        }

        self.db.get_raw_transaction(hash)
    }

    fn get_transaction(&self, hash: TxHash) -> RepositoryResult<Option<ZkTransaction>> {
        if let Some(tx) = self.in_memory.get_transaction(hash)? {
            return Ok(Some(tx));
        }

        self.db.get_transaction(hash)
    }

    fn get_transaction_receipt(&self, hash: TxHash) -> RepositoryResult<Option<ZkReceiptEnvelope>> {
        if let Some(receipt) = self.in_memory.get_transaction_receipt(hash)? {
            return Ok(Some(receipt));
        }

        self.db.get_transaction_receipt(hash)
    }

    fn get_transaction_meta(&self, hash: TxHash) -> RepositoryResult<Option<TxMeta>> {
        if let Some(meta) = self.in_memory.get_transaction_meta(hash)? {
            return Ok(Some(meta));
        }

        self.db.get_transaction_meta(hash)
    }

    fn get_transaction_hash_by_sender_nonce(
        &self,
        sender: Address,
        nonce: TxNonce,
    ) -> RepositoryResult<Option<TxHash>> {
        if let Some(tx_hash) = self
            .in_memory
            .get_transaction_hash_by_sender_nonce(sender, nonce)?
        {
            return Ok(Some(tx_hash));
        }

        self.db.get_transaction_hash_by_sender_nonce(sender, nonce)
    }

    fn get_stored_transaction(&self, hash: TxHash) -> RepositoryResult<Option<StoredTxData>> {
        if let Some(stored_tx) = self.in_memory.get_stored_transaction(hash)? {
            return Ok(Some(stored_tx));
        }

        self.db.get_stored_transaction(hash)
    }

    fn get_latest_block(&self) -> u64 {
        self.in_memory
            .get_latest_block()
            .max(self.db.get_latest_block())
    }
}

impl WriteRepository for RepositoryManager {
    async fn populate(
        &self,
        block_output: &BlockOutput,
        transactions: Vec<ZkTransaction>,
        failed_transactions: Vec<(TxHash, InvalidTransaction)>,
    ) -> RepositoryResult<()> {
        if !self.db_ready_to_process_blocks.load(Ordering::Relaxed) {
            if block_output.header.number > 0 {
                self.db.rollback(block_output.header.number - 1)?;
            }

            self.db_ready_to_process_blocks
                .store(true, Ordering::Relaxed);
            tracing::info!("Repo DB is ready to process blocks");
        }

        // Hard memory bound: block production waits for the persist loop's PRUNING to keep the
        // in-memory window at `max_blocks_in_memory` (persistence itself runs far ahead of
        // pruning — see `run_persist_loop`).
        let should_be_pruned_up_to = self
            .in_memory
            .get_latest_block()
            .saturating_sub(self.max_blocks_in_memory);
        if should_be_pruned_up_to > 0 {
            let _ = self
                .pruned_block_number
                .subscribe()
                .wait_for(|pruned| *pruned >= should_be_pruned_up_to)
                .await;
        }
        let (block, transactions) = self
            .in_memory
            .populate_in_memory(block_output, transactions);

        // todo: move notifications upstream of `RepositoryManager`
        let notification = BlockNotification {
            block,
            transactions: transactions
                .iter()
                .map(|data| (*data.tx.hash(), data.clone()))
                .collect(),
            failed_transactions: Arc::new(failed_transactions.into_iter().collect()),
        };
        // Ignore error if there are no subscribed receivers
        let _ = self.block_sender.send(notification);
        Ok(())
    }
}

impl SubscribeToBlocks for RepositoryManager {
    fn subscribe_to_blocks(&self) -> broadcast::Receiver<BlockNotification> {
        self.block_sender.subscribe()
    }
}
