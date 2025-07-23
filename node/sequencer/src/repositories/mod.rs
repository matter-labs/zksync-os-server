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
pub mod api_interface;
pub mod block_receipt_repository;
pub mod bytecode_property_respository;
mod db;
mod metrics;
pub mod notifications;
pub mod transaction_receipt_repository;

use crate::metrics::GENERAL_METRICS;
use crate::repositories::{
    account_property_repository::extract_account_properties,
    bytecode_property_respository::BytecodeRepository,
    db::{RepositoryCF, RepositoryDB},
    metrics::REPOSITORIES_METRICS,
    notifications::{BlockNotification, SubscribeToBlocks},
    transaction_receipt_repository::{TransactionReceiptRepository, transaction_to_api_data},
};
pub use account_property_repository::AccountPropertyRepository;
use alloy::primitives::{BlockHash, Bloom, Sealed, TxHash};
pub use block_receipt_repository::BlockReceiptRepository;
use std::ops::Div;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};
use zk_os_forward_system::run::BatchOutput;
use zksync_os_types::ZkTransaction;
use zksync_storage::RocksDB;

/// Size of the broadcast channel used to notify about new blocks.
const BLOCK_NOTIFICATION_CHANNEL_SIZE: usize = 256;

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
    // TODO: get rid of `account_property_repository` and `bytecode_repository`
    pub account_property_repository: AccountPropertyRepository,
    pub bytecode_repository: BytecodeRepository,

    block_receipt_repository: BlockReceiptRepository,
    transaction_receipt_repository: TransactionReceiptRepository,

    db: RepositoryDB,
    latest_block: watch::Sender<u64>,
    max_blocks_in_memory: u64,
    block_sender: broadcast::Sender<BlockNotification>,
}

impl RepositoryManager {
    pub fn new(blocks_to_retain: usize, db_path: PathBuf) -> Self {
        let db = RocksDB::<RepositoryCF>::new(&db_path).expect("Failed to open db");
        let db = RepositoryDB::new(db);
        let db_block_number = db.latest_block_number();
        let (block_sender, _) = broadcast::channel(BLOCK_NOTIFICATION_CHANNEL_SIZE);

        RepositoryManager {
            account_property_repository: AccountPropertyRepository::new(blocks_to_retain),
            bytecode_repository: BytecodeRepository::new(blocks_to_retain),
            block_receipt_repository: BlockReceiptRepository::new(),
            transaction_receipt_repository: TransactionReceiptRepository::new(),
            db,
            latest_block: watch::channel(db_block_number).0,
            max_blocks_in_memory: blocks_to_retain as u64,
            block_sender,
        }
    }

    /// Calls `populate_in_memory` while respecting `self.max_blocks_in_memory`.
    /// Blocks until the database has enough blocks persisted to allow in-memory population.
    pub async fn populate_in_memory_blocking(
        &self,
        block_output: BatchOutput,
        transactions: Vec<ZkTransaction>,
    ) {
        let should_be_persisted_up_to = self
            .latest_block
            .borrow()
            .saturating_sub(self.max_blocks_in_memory);
        let _ = self
            .db
            .wait_for_block_number(should_be_persisted_up_to)
            .await;
        self.populate_in_memory(block_output, transactions);
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
    fn populate_in_memory(&self, mut block_output: BatchOutput, transactions: Vec<ZkTransaction>) {
        let total_latency = REPOSITORIES_METRICS.insert_block[&"total"].start();
        let block_number = block_output.header.number;
        let tx_count = transactions.len();
        let tx_hashes = transactions
            .iter()
            .map(|tx| TxHash::from(tx.hash().0))
            .collect();

        // Drop rejected transactions from the block output
        block_output.tx_results.retain(|result| result.is_ok());

        // Extract account properties from the block output
        let (account_properties, bytecodes) = extract_account_properties(&block_output);

        // Add account properties to the account property repository
        let account_properties_latency_observer =
            REPOSITORIES_METRICS.insert_block[&"account_properties"].start();
        self.account_property_repository
            .add_diff(block_number, account_properties);
        let account_properties_latency = account_properties_latency_observer.observe();

        // Add bytecodes to the bytecode repository
        let bytecodes_latency_observer = REPOSITORIES_METRICS.insert_block[&"bytecodes"].start();
        self.bytecode_repository.add_diff(block_number, bytecodes);
        let bytecodes_latency = bytecodes_latency_observer.observe();

        // Add transaction receipts to the transaction receipt repository
        let mut log_index = 0;
        let mut block_bloom = Bloom::default();
        let mut stored_txs = Vec::new();
        let mut notification_transactions = Vec::new();
        let hash = BlockHash::from(block_output.header.hash());
        let sealed_block_output = Sealed::new_unchecked(block_output, hash);
        for (tx_index, tx) in transactions.into_iter().enumerate() {
            let tx_hash = *tx.hash();
            let stored_tx = transaction_to_api_data(&sealed_block_output, tx_index, log_index, tx);
            notification_transactions.push(stored_tx.clone());
            log_index += stored_tx.receipt.logs().len() as u64;
            block_bloom.accrue_bloom(stored_tx.receipt.logs_bloom());
            // todo: consider saving `Arc<StoredTxData>` instead to share them with `BlockNotification` below
            //       we clone stored transactions in every fetch method anyway so little reason not to `Arc` them
            stored_txs.push((tx_hash, stored_tx));
        }
        let (mut block_output, hash) = sealed_block_output.into_parts();
        block_output.header.logs_bloom = block_bloom.into_array();
        let block_header = Sealed::new_unchecked(block_output.header, hash);

        // Add data to repositories.
        let transaction_receipts_latency_observer =
            REPOSITORIES_METRICS.insert_block[&"transaction_receipts"].start();
        self.transaction_receipt_repository.insert(stored_txs);
        let transaction_receipts_latency = transaction_receipts_latency_observer.observe();

        let block_receipt_latency_observer =
            REPOSITORIES_METRICS.insert_block[&"block_receipts"].start();
        self.block_receipt_repository
            .insert(&block_header, tx_hashes);
        let block_receipt_latency = block_receipt_latency_observer.observe();

        self.latest_block.send_replace(block_number);

        let notification = BlockNotification {
            header: Arc::new(
                block_receipt_repository::alloy_header(&block_header).seal(block_header.hash()),
            ),
            transactions: Arc::new(notification_transactions),
        };
        // Ignore error if there are no subscribed receivers
        let _ = self.block_sender.send(notification);

        let latency = total_latency.observe();
        REPOSITORIES_METRICS
            .insert_block_per_tx
            .observe(latency.div(tx_count as u32));

        tracing::debug!(
            block_number,
            total_latency = latency,
            account_properties_latency,
            bytecodes_latency,
            transaction_receipts_latency,
            block_receipt_latency,
            "Stored a block in memory with {} transactions",
            tx_count,
        );
    }

    pub async fn run_persist_loop(&self) {
        loop {
            let db_block_number = self.db.latest_block_number();
            let _ = self
                .latest_block
                .subscribe()
                .wait_for(|value| *value > db_block_number)
                .await
                .unwrap();

            let number = db_block_number + 1;
            let block = self
                .block_receipt_repository
                .get_by_number(number)
                .expect("Missing block receipt");
            let txs = self
                .transaction_receipt_repository
                .get_by_hashes(&block.body.transactions)
                .unwrap();

            let persist_latency_observer = REPOSITORIES_METRICS.persist_block.start();
            self.db.write_block(&block, &txs);
            let persist_latency = persist_latency_observer.observe();
            REPOSITORIES_METRICS
                .persist_block_per_tx
                .observe(persist_latency.div(txs.len() as u32));

            self.block_receipt_repository.remove_by_number(number);
            self.transaction_receipt_repository
                .remove_by_hashes(&block.body.transactions);
            tracing::info!(
               block_number=number, 
               persist_latency,
               "Persisted receipts",
            );

            REPOSITORIES_METRICS
                .persistence_lag
                .set(self.latest_block.borrow().saturating_sub(number) as usize);
            GENERAL_METRICS.block_number[&"persist"].set(number);
        }
    }
}

impl SubscribeToBlocks for RepositoryManager {
    fn subscribe_to_blocks(&self) -> broadcast::Receiver<BlockNotification> {
        self.block_sender.subscribe()
    }
}
