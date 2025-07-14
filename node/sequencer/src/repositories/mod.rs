//! Repository module containing extracted storage logic from StateHandle
//!
//! This module provides three main repository types:
//! - AccountPropertyRepository: History-based storage for account properties with compaction
//! - BlockReceiptRepository: LRU cache for block receipts
//! - TransactionReceiptRepository: Indefinite storage for transaction receipts
//!
//! Additionally, it provides a RepositoryManager that holds all three repositories
//! and provides unified methods for managing block outputs.

pub mod account_property_repository;
pub mod block_receipt_repository;
mod db;
mod metrics;
pub mod transaction_receipt_repository;

use crate::repositories::account_property_repository::extract_account_properties;
use crate::repositories::db::{RepositoryCF, RepositoryDB};
use crate::repositories::metrics::REPOSITORIES_METRICS;
use crate::repositories::transaction_receipt_repository::{
    l1_transaction_to_api_data, l2_transaction_to_api_data, StoredTxData,
};
pub use account_property_repository::AccountPropertyRepository;
use alloy::consensus::{Block, ReceiptEnvelope};
use alloy::primitives::TxHash;
pub use block_receipt_repository::BlockReceiptRepository;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::watch;
pub use transaction_receipt_repository::TransactionReceiptRepository;
use zk_os_forward_system::run::BatchOutput;
use zksync_os_types::{L1Transaction, L2Transaction};
use zksync_storage::RocksDB;

/// Manages repositories that store node data required for RPC but not for VM execution.
///
/// This includes auxiliary data such as block and transaction receipts, and account-specific metadata
/// that are necessary for exposing historical and current information via RPC.
///
/// Note:
/// - This component does **not** manage the canonical `State` (i.e., the data required for VM execution - storage slots and preimages).
/// - No atomicity guarantees are provided between repository updates.

#[derive(Clone, Debug)]
pub struct RepositoryManager {
    // TODO: get rid of `account_property_repository`
    pub account_property_repository: AccountPropertyRepository,

    block_receipt_repository: BlockReceiptRepository,
    transaction_receipt_repository: TransactionReceiptRepository,

    db: RepositoryDB,
    latest_block: watch::Sender<u64>,
    max_blocks_in_memory: u64,
    poll_interval: Duration,
}

impl RepositoryManager {
    pub fn new(blocks_to_retain: usize, db_path: PathBuf) -> Self {
        let db = RocksDB::<RepositoryCF>::new(&db_path).expect("Failed to open db");
        let db = RepositoryDB::new(db);
        let db_block_number = db.latest_block_number();

        RepositoryManager {
            account_property_repository: AccountPropertyRepository::new(blocks_to_retain),
            block_receipt_repository: BlockReceiptRepository::new(),
            transaction_receipt_repository: TransactionReceiptRepository::new(),
            db,
            latest_block: watch::channel(db_block_number).0,
            max_blocks_in_memory: blocks_to_retain as u64,
            poll_interval: Duration::from_millis(10),
        }
    }

    /// Calls `populate_in_memory` while respecting `self.max_blocks_in_memory`.
    /// Blocks until the database has enough blocks persisted to allow in-memory population.
    pub async fn populate_in_memory_blocking(
        &self,
        block_output: BatchOutput,
        l1_transactions: Vec<L1Transaction>,
        l2_transactions: Vec<L2Transaction>,
    ) {
        let mut timer = tokio::time::interval(self.poll_interval);

        let mut db_block_number = self.db.latest_block_number();
        let should_be_persisted_up_to = self
            .latest_block
            .borrow()
            .saturating_sub(self.max_blocks_in_memory);
        while db_block_number < should_be_persisted_up_to {
            timer.tick().await;
            db_block_number = self.db.latest_block_number()
        }
        self.populate_in_memory(block_output, l1_transactions, l2_transactions);
    }

    /// Adds a block's output to all relevant repositories.
    ///
    /// This method processes a `BatchOutput` and distributes its contents across the appropriate
    /// repositories:
    /// - Extracts account properties and stores them in `AccountPropertyRepository`.
    /// - Stores the full `BatchOutput` in `BlockReceiptRepository`.
    /// - Generates transaction receipts and stores them in `TransactionReceiptRepository`.
    ///
    /// Notes:
    /// - No atomicity or ordering guarantees are provided for repository updates.
    /// - Upon successful return, all repositories are considered up to date at `block_number`.
    fn populate_in_memory(
        &self,
        mut block_output: BatchOutput,
        l1_transactions: Vec<L1Transaction>,
        l2_transactions: Vec<L2Transaction>,
    ) {
        let latency = REPOSITORIES_METRICS.insert_block[&"total"].start();
        let block_number = block_output.header.number;
        let tx_hashes = l1_transactions
            .iter()
            .map(|tx| TxHash::from(tx.hash().0))
            .chain(l2_transactions.iter().map(|tx| *tx.hash()))
            .collect();

        // Drop rejected transactions from the block output
        block_output.tx_results.retain(|result| result.is_ok());

        // Extract account properties from the block output
        let account_properties = extract_account_properties(&block_output);

        // Add account properties to the account property repository
        self.account_property_repository
            .add_diff(block_number, account_properties);

        // Add transaction receipts to the transaction receipt repository
        let mut tx_index = 0;
        let mut log_index = 0;
        let mut block_bloom = alloy::primitives::Bloom::default();

        for l1_tx in l1_transactions.into_iter() {
            let hash = TxHash::from(l1_tx.hash().0);
            let stored_tx = l1_transaction_to_api_data(&block_output, tx_index, log_index, l1_tx);
            tx_index += 1;
            log_index += stored_tx.receipt.logs().len() as u64;
            block_bloom.accrue_bloom(stored_tx.receipt.logs_bloom());
            self.transaction_receipt_repository.insert(hash, stored_tx);
        }

        for l2_tx in l2_transactions.into_iter() {
            let hash = TxHash::from(l2_tx.hash().0);
            let stored_tx = l2_transaction_to_api_data(&block_output, tx_index, log_index, l2_tx);
            tx_index += 1;
            log_index += stored_tx.receipt.logs().len() as u64;
            block_bloom.accrue_bloom(stored_tx.receipt.logs_bloom());
            self.transaction_receipt_repository.insert(hash, stored_tx);
        }
        block_output.header.logs_bloom = block_bloom.into_array();

        // Add the full block output to the block receipt repository
        self.block_receipt_repository
            .insert(&block_output.header, tx_hashes);
        self.latest_block.send_replace(block_number);
        latency.observe();
    }

    pub async fn run_persist_loop(&self) {
        let mut timer = tokio::time::interval(self.poll_interval);

        loop {
            timer.tick().await;

            let db_block_number = self.db.latest_block_number();
            let latest_block = *self.latest_block.borrow();
            for number in (db_block_number + 1)..=latest_block {
                let block = self
                    .block_receipt_repository
                    .get_by_number(number)
                    .expect("Missing block receipt");
                let txs = self
                    .transaction_receipt_repository
                    .get_by_hashes(&block.body.transactions);
                self.db.write_block(&block, &txs);

                self.block_receipt_repository.remove_by_number(number);
                self.transaction_receipt_repository
                    .remove_by_hashes(&block.body.transactions);
                tracing::info!(number, "Persisted receipts");
            }
        }
    }

    pub fn get_block_by_number(&self, number: u64) -> Option<Block<TxHash>> {
        if let Some(res) = self.block_receipt_repository.get_by_number(number) {
            return Some(res);
        }

        self.db.get_block_by_number(number)
    }

    pub fn get_tx_by_hash(&self, tx_hash: TxHash) -> Option<L2Transaction> {
        if let Some(res) = self.transaction_receipt_repository.get_tx_by_hash(tx_hash) {
            return Some(res);
        }

        self.db.get_tx_by_hash(tx_hash)
    }

    pub fn get_tx_receipt_by_hash(&self, tx_hash: TxHash) -> Option<ReceiptEnvelope> {
        if let Some(res) = self
            .transaction_receipt_repository
            .get_receipt_by_hash(tx_hash)
        {
            return Some(res);
        }

        self.db.get_tx_receipt_by_hash(tx_hash)
    }

    pub fn get_stored_tx_by_hash(&self, tx_hash: TxHash) -> Option<StoredTxData> {
        if let Some(res) = self
            .transaction_receipt_repository
            .get_stored_tx_by_hash(tx_hash)
        {
            return Some(res);
        }

        self.db.get_stored_tx_by_hash(tx_hash)
    }

    pub fn get_canonized_block(&self) -> u64 {
        *self.latest_block.borrow()
    }
}
