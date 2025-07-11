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
mod metrics;
pub mod transaction_receipt_repository;

use crate::repositories::account_property_repository::extract_account_properties;
use crate::repositories::metrics::REPOSITORIES_METRICS;
use crate::repositories::transaction_receipt_repository::transaction_to_api_data;
pub use account_property_repository::AccountPropertyRepository;
pub use block_receipt_repository::BlockReceiptRepository;
pub use transaction_receipt_repository::TransactionReceiptRepository;
use zk_ee::utils::Bytes32;
use zk_os_forward_system::run::BatchOutput;
use zksync_os_types::ZkTransaction;

/// Manages repositories that store node data required for RPC but not for VM execution.
///
/// This includes auxiliary data such as block and transaction receipts, and account-specific metadata
/// that are necessary for exposing historical and current information via RPC.
///
/// Note:
/// - This component does **not** manage the canonical `State` (i.e., the data required for VM execution - storage slots and preimages).
/// - No atomicity guarantees are provided between repository updates.
/// - Block tip tracking is **not** handled here. It should be maintained separately and advanced
///   only after a successful invocation of `add_block_output_to_repos`.

#[derive(Clone, Debug)]
pub struct RepositoryManager {
    pub account_property_repository: AccountPropertyRepository,
    pub block_receipt_repository: BlockReceiptRepository,
    pub transaction_receipt_repository: TransactionReceiptRepository,
}

impl RepositoryManager {
    pub fn new(blocks_to_retain: usize) -> Self {
        RepositoryManager {
            account_property_repository: AccountPropertyRepository::new(blocks_to_retain),
            block_receipt_repository: BlockReceiptRepository::new(blocks_to_retain),
            transaction_receipt_repository: TransactionReceiptRepository::new(),
        }
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
    pub fn add_block_output_to_repos(
        &self,
        block_number: u64,
        mut block_output: BatchOutput,
        transactions: Vec<ZkTransaction>,
    ) {
        let latency = REPOSITORIES_METRICS.insert_block[&"total"].start();

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
        let mut tx_hashes = Vec::new();
        for tx in transactions {
            let tx_hash = *tx.hash();
            tx_hashes.push(tx_hash);
            let tx_hash = Bytes32::from(tx_hash.0);
            let api_tx = transaction_to_api_data(&block_output, tx_index, log_index, tx);
            log_index += api_tx.receipt.logs().len();
            tx_index += 1;
            block_bloom.accrue_bloom(api_tx.receipt.inner.logs_bloom());
            self.transaction_receipt_repository.insert(tx_hash, api_tx);
        }
        block_output.header.logs_bloom = block_bloom.into_array();

        // Add the full block output to the block receipt repository
        self.block_receipt_repository
            .insert(block_number, block_output, tx_hashes);
        latency.observe();
    }
}
