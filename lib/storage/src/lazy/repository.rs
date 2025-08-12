use crate::db::RepositoryDb;
use crate::in_memory::RepositoryInMemory;
use crate::metrics::REPOSITORIES_METRICS;
use alloy::primitives::{Address, BlockHash, BlockNumber, TxHash, TxNonce};
use std::ops::Div;
use std::path::PathBuf;
use tokio::sync::broadcast;
use zk_os_forward_system::run::BlockOutput;
use zksync_os_storage_api::notifications::{BlockNotification, SubscribeToBlocks};
use zksync_os_storage_api::{
    ReadRepository, RepositoryBlock, RepositoryResult, StoredTxData, TxMeta,
};
use zksync_os_types::{ZkReceiptEnvelope, ZkTransaction};

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
}

impl RepositoryManager {
    pub fn new(blocks_to_retain: usize, db_path: PathBuf, genesis: RepositoryBlock) -> Self {
        let db = RepositoryDb::new(&db_path);
        let (block_sender, _) = broadcast::channel(BLOCK_NOTIFICATION_CHANNEL_SIZE);

        RepositoryManager {
            // Initializes in-memory repository with genesis block. It is never pruned from cache.
            in_memory: RepositoryInMemory::new(genesis),
            db,
            max_blocks_in_memory: blocks_to_retain as u64,
            block_sender,
        }
    }

    /// Calls `populate_in_memory` while respecting `self.max_blocks_in_memory`.
    /// Blocks until the database has enough blocks persisted to allow in-memory population.
    pub async fn populate_in_memory_blocking(
        &self,
        block_output: BlockOutput,
        transactions: Vec<ZkTransaction>,
    ) {
        let should_be_persisted_up_to = self
            .in_memory
            .get_latest_block()
            .saturating_sub(self.max_blocks_in_memory);
        let _ = self
            .db
            .wait_for_block_number(should_be_persisted_up_to)
            .await;
        let (block, transactions) = self
            .in_memory
            .populate_in_memory(block_output, transactions);

        // todo: move notifications upstream of `RepositoryManager`
        let notification = BlockNotification {
            block,
            transactions,
        };
        // Ignore error if there are no subscribed receivers
        let _ = self.block_sender.send(notification);
    }

    // fixme: as this loop is not tied to state compacting, it can fall behind and result in
    //        unrecoverable state on restart
    pub async fn run_persist_loop(&self) {
        loop {
            let db_block_number = self.db.get_latest_block();
            self.in_memory
                .wait_for_block_number(db_block_number + 1)
                .await;

            let block_number = db_block_number + 1;
            let (block, txs) = self
                .in_memory
                .get_block_and_transactions_by_number(block_number)
                .expect("missing in-memory block and/or transactions");

            let persist_latency_observer = REPOSITORIES_METRICS.persist_block.start();
            self.db.write_block(&block, &txs);
            let persist_latency = persist_latency_observer.observe();
            REPOSITORIES_METRICS
                .persist_block_per_tx
                .observe(persist_latency.div(txs.len() as u32));

            self.in_memory
                .remove_block_and_transactions(block_number, &block.body.transactions);

            let persistence_lag = self
                .in_memory
                .get_latest_block()
                .saturating_sub(block_number) as usize;
            REPOSITORIES_METRICS.persistence_lag.set(persistence_lag);
            tracing::info!(
                block_number,
                ?persist_latency,
                persistence_lag,
                "persisted block",
            );

            REPOSITORIES_METRICS.persist_block_number.set(block_number);
        }
    }

    pub fn get_latest_persisted_block(&self) -> u64 {
        self.db.get_latest_block()
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
        self.in_memory.get_latest_block()
    }
}

impl SubscribeToBlocks for RepositoryManager {
    fn subscribe_to_blocks(&self) -> broadcast::Receiver<BlockNotification> {
        self.block_sender.subscribe()
    }
}
