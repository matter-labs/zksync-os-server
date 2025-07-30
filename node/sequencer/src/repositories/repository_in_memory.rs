use crate::repositories::metrics::REPOSITORIES_METRICS;
use crate::repositories::transaction_receipt_repository::{
    TransactionReceiptRepository, transaction_to_api_data,
};
use crate::repositories::{BlockReceiptRepository, block_receipt_repository};
use alloy::consensus::Sealed;
use alloy::eips::Encodable2718;
use alloy::primitives::{Address, B256, BlockHash, BlockNumber, Bloom, TxHash, TxNonce};
use std::sync::Arc;
use tokio::sync::watch;
use zk_os_forward_system::run::BlockOutput;
use zksync_os_storage_api::{
    ReadRepository, RepositoryBlock, RepositoryResult, StoredTxData, TxMeta,
};
use zksync_os_types::{ZkReceiptEnvelope, ZkTransaction};

/// In-memory repositories that store node data required for RPC but not for VM execution.
///
/// This includes auxiliary data such as block and transaction receipts, and account-specific metadata
/// that are necessary for exposing historical and current information via RPC.
///
/// Note:
/// - This component does **not** manage the canonical `State` (i.e., the data required for VM execution - storage slots and preimages).
/// - No atomicity guarantees are provided between repository updates.
#[derive(Clone, Debug)]
pub struct RepositoryInMemory {
    block_receipt_repository: BlockReceiptRepository,
    transaction_receipt_repository: TransactionReceiptRepository,
    latest_block: watch::Sender<u64>,
}

impl RepositoryInMemory {
    pub fn new(latest_block: u64) -> Self {
        Self {
            block_receipt_repository: BlockReceiptRepository::new(),
            transaction_receipt_repository: TransactionReceiptRepository::new(),
            latest_block: watch::channel(latest_block).0,
        }
    }

    /// Waits until the latest block number is at least `block_number`.
    /// Returns the latest block number once it is reached.
    pub async fn wait_for_block_number(&self, block_number: u64) -> u64 {
        *self
            .latest_block
            .subscribe()
            .wait_for(|value| *value >= block_number)
            .await
            .unwrap()
    }

    /// Adds a block's output to all relevant repositories.
    ///
    /// This method processes a `BlockOutput` and distributes its contents across the appropriate
    /// repositories:
    /// - Stores the block in `BlockReceiptRepository`.
    /// - Generates transaction receipts and stores them in `TransactionReceiptRepository`.
    ///
    /// Notes:
    /// - No atomicity or ordering guarantees are provided for repository updates.
    /// - Upon successful return, all repositories are considered up to date at `block_number`.
    pub fn populate_in_memory(
        &self,
        mut block_output: BlockOutput,
        transactions: Vec<ZkTransaction>,
    ) -> (Arc<RepositoryBlock>, Vec<(B256, Arc<StoredTxData>)>) {
        let total_latency_observer = REPOSITORIES_METRICS.insert_block[&"total"].start();
        let block_number = block_output.header.number;
        let tx_count = transactions.len();
        let tx_hashes = transactions
            .iter()
            .map(|tx| TxHash::from(tx.hash().0))
            .collect();

        // Drop rejected transactions from the block output
        block_output.tx_results.retain(|result| result.is_ok());

        // Add transaction receipts to the transaction receipt repository
        let mut log_index = 0;
        let mut block_bloom = Bloom::default();
        let mut stored_txs = Vec::new();
        let hash = BlockHash::from(block_output.header.hash());
        let sealed_block_output = Sealed::new_unchecked(block_output, hash);
        for (tx_index, tx) in transactions.into_iter().enumerate() {
            let tx_hash = *tx.hash();
            let stored_tx = Arc::new(transaction_to_api_data(
                &sealed_block_output,
                tx_index,
                log_index,
                tx,
            ));
            log_index += stored_tx.receipt.logs().len() as u64;
            block_bloom.accrue_bloom(stored_tx.receipt.logs_bloom());
            stored_txs.push((tx_hash, stored_tx));
        }
        let (mut block_output, hash) = sealed_block_output.into_parts();
        block_output.header.logs_bloom = block_bloom.into_array();
        let block = Arc::new(Sealed::new_unchecked(
            alloy::consensus::Block {
                header: block_receipt_repository::alloy_header(&block_output.header),
                body: alloy::consensus::BlockBody {
                    transactions: tx_hashes,
                    ommers: vec![],
                    withdrawals: None,
                },
            },
            hash,
        ));

        // Add data to repositories.
        let transaction_receipts_latency_observer =
            REPOSITORIES_METRICS.insert_block[&"transaction_receipts"].start();
        self.transaction_receipt_repository.insert(&stored_txs);
        let transaction_receipts_latency = transaction_receipts_latency_observer.observe();

        let block_receipt_latency_observer =
            REPOSITORIES_METRICS.insert_block[&"block_receipts"].start();
        self.block_receipt_repository.insert(block.clone());
        let block_receipt_latency = block_receipt_latency_observer.observe();

        self.latest_block.send_replace(block_number);

        let latency = total_latency_observer.observe();
        REPOSITORIES_METRICS
            .insert_block_per_tx
            .observe(latency / (tx_count as u32));

        REPOSITORIES_METRICS
            .in_memory_txs_count
            .set(self.transaction_receipt_repository.len());

        tracing::debug!(
            block_number,
            total_latency = ?latency,
            ?transaction_receipts_latency,
            ?block_receipt_latency,
            "Stored a block in memory with {} transactions",
            tx_count,
        );

        (block, stored_txs)
    }

    pub fn get_block_and_transactions_by_number(
        &self,
        block_number: BlockNumber,
    ) -> Option<(RepositoryBlock, Vec<Arc<StoredTxData>>)> {
        let block = self.block_receipt_repository.get_by_number(block_number)?;
        let txs = self
            .transaction_receipt_repository
            .get_by_hashes(&block.body.transactions)?;
        Some((block, txs))
    }

    pub fn remove_block_and_transactions(&self, block_number: BlockNumber, tx_hashes: &[TxHash]) {
        self.block_receipt_repository.remove_by_number(block_number);
        self.transaction_receipt_repository
            .remove_by_hashes(tx_hashes);
    }
}

impl ReadRepository for RepositoryInMemory {
    fn get_block_by_number(
        &self,
        number: BlockNumber,
    ) -> RepositoryResult<Option<RepositoryBlock>> {
        Ok(self.block_receipt_repository.get_by_number(number))
    }

    fn get_block_by_hash(&self, hash: BlockHash) -> RepositoryResult<Option<RepositoryBlock>> {
        Ok(self.block_receipt_repository.get_by_hash(hash))
    }

    fn get_raw_transaction(&self, hash: TxHash) -> RepositoryResult<Option<Vec<u8>>> {
        Ok(self
            .get_transaction(hash)?
            .map(|tx| tx.into_envelope().encoded_2718()))
    }

    fn get_transaction(&self, hash: TxHash) -> RepositoryResult<Option<ZkTransaction>> {
        Ok(self.transaction_receipt_repository.get_transaction(hash))
    }

    fn get_transaction_receipt(&self, hash: TxHash) -> RepositoryResult<Option<ZkReceiptEnvelope>> {
        Ok(self
            .transaction_receipt_repository
            .get_transaction_receipt(hash))
    }

    fn get_transaction_meta(&self, hash: TxHash) -> RepositoryResult<Option<TxMeta>> {
        Ok(self
            .transaction_receipt_repository
            .get_transaction_meta(hash))
    }

    fn get_transaction_hash_by_sender_nonce(
        &self,
        sender: Address,
        nonce: TxNonce,
    ) -> RepositoryResult<Option<TxHash>> {
        Ok(self
            .transaction_receipt_repository
            .get_transaction_hash_by_sender_nonce(sender, nonce))
    }

    fn get_stored_transaction(&self, hash: TxHash) -> RepositoryResult<Option<StoredTxData>> {
        Ok(self
            .transaction_receipt_repository
            .get_stored_tx_by_hash(hash))
    }

    fn get_latest_block(&self) -> u64 {
        *self.latest_block.borrow()
    }
}
