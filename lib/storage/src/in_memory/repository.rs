use crate::metrics::REPOSITORIES_METRICS;
use alloy::consensus::{Sealed, Transaction};
use alloy::eips::Encodable2718;
use alloy::primitives::{Address, B256, BlockHash, BlockNumber, Bloom, TxHash, TxNonce};
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::watch;
use zksync_os_interface::types::{BlockOutput, ExecutionResult};
use zksync_os_storage_api::{
    ReadRepository, RepositoryBlock, RepositoryResult, StoredTxData, TxMeta,
};
use zksync_os_types::{L2ToL1Log, ZkReceipt, ZkReceiptEnvelope, ZkTransaction};

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
    /// Latest block number that's guaranteed to be present in all the repositories
    latest_block: watch::Sender<u64>,
}

impl RepositoryInMemory {
    /// Initialize with genesis
    pub fn new(genesis: RepositoryBlock) -> Self {
        assert_eq!(genesis.number, 0);
        let block_receipt_repository = BlockReceiptRepository::new();
        block_receipt_repository.insert(Arc::new(genesis));
        Self {
            block_receipt_repository,
            transaction_receipt_repository: TransactionReceiptRepository::new(),
            latest_block: watch::channel(0).0,
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
        let mut cumulative_gas_used = 0;
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
                cumulative_gas_used,
                tx,
            ));
            log_index += stored_tx.receipt.logs().len() as u64;
            cumulative_gas_used += stored_tx.meta.gas_used;
            block_bloom.accrue_bloom(stored_tx.receipt.logs_bloom());
            stored_txs.push((tx_hash, stored_tx));
        }
        let (block_output, hash) = sealed_block_output.into_parts();
        let header = {
            let mut h = block_output.header.unseal();
            h.logs_bloom = block_bloom;
            h
        };
        let block = Arc::new(Sealed::new_unchecked(
            alloy::consensus::Block {
                header,
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

        let total_latency = total_latency_observer.observe();
        REPOSITORIES_METRICS
            .insert_block_per_tx
            .observe(total_latency / (tx_count.max(1) as u32));

        REPOSITORIES_METRICS
            .in_memory_txs_count
            .set(self.transaction_receipt_repository.len());

        tracing::debug!(
            block_number,
            tx_count,
            ?total_latency,
            ?transaction_receipts_latency,
            ?block_receipt_latency,
            "stored block in memory",
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

/// In-memory repository of the most recent N `BlockOutput`s, keyed by block number.
///
/// Inserts must happen in strictly ascending order.
///
#[derive(Clone, Debug, Default)]
struct BlockReceiptRepository {
    /// Map from block number → block hash.
    hash_index: Arc<DashMap<BlockNumber, BlockHash>>,
    /// Map from block hash → block.
    receipts: Arc<DashMap<BlockHash, Arc<Sealed<alloy::consensus::Block<TxHash>>>>>,
}

impl BlockReceiptRepository {
    /// Create a new repository.
    pub fn new() -> Self {
        BlockReceiptRepository::default()
    }

    /// Insert the `BlockOutput` for `block`.
    ///
    /// Must be called with `block == latest_block() + 1`.
    pub fn insert(&self, block: Arc<Sealed<alloy::consensus::Block<TxHash>>>) {
        let number = block.number;
        let hash = block.hash();
        self.receipts.insert(hash, block);
        self.hash_index.insert(number, hash);
    }

    /// Retrieve the block by its number, if present.
    pub fn get_by_number(&self, number: BlockNumber) -> Option<RepositoryBlock> {
        let hash = *self.hash_index.get(&number)?;
        self.get_by_hash(hash)
    }

    /// Retrieve the block by its hash, if present.
    pub fn get_by_hash(&self, hash: BlockHash) -> Option<RepositoryBlock> {
        self.receipts.get(&hash).map(|r| r.value().as_ref().clone())
    }

    pub fn remove_by_number(&self, number: BlockNumber) {
        if let Some((_, hash)) = self.hash_index.remove(&number) {
            self.receipts.remove(&hash);
        }
    }
}

/// Thread-safe in-memory repository of transaction receipts, keyed by transaction hash.
///
/// Retains all inserted receipts indefinitely. Internally uses a lock-free
/// DashMap to allow concurrent inserts and lookups.
///
/// todo: unbounded memory use
#[derive(Clone, Debug)]
struct TransactionReceiptRepository {
    /// Map from tx hash → (tx, receipt, meta).
    tx_data: Arc<DashMap<TxHash, Arc<StoredTxData>>>,
    /// Map from (sender, nonce) → tx hash.
    sender_nonce_index: Arc<DashMap<(Address, TxNonce), TxHash>>,
}

impl TransactionReceiptRepository {
    /// Creates a new repository.
    pub fn new() -> Self {
        TransactionReceiptRepository {
            tx_data: Arc::new(DashMap::new()),
            sender_nonce_index: Arc::new(DashMap::new()),
        }
    }

    /// Inserts data for multiple txs. If a data for the same hash
    /// already exists, it will be overwritten.
    pub fn insert(&self, txs: &[(TxHash, Arc<StoredTxData>)]) {
        for (tx_hash, data) in txs {
            let sender = data.tx.signer();
            let nonce = data.tx.nonce();
            self.tx_data.insert(*tx_hash, data.clone());
            self.sender_nonce_index.insert((sender, nonce), *tx_hash);
        }
    }

    /// Retrieves transaction by its hash, if present.
    pub fn get_transaction(&self, tx_hash: TxHash) -> Option<ZkTransaction> {
        self.tx_data.get(&tx_hash).map(|r| r.value().tx.clone())
    }

    /// Retrieves transaction receipt by its hash, if present.
    pub fn get_transaction_receipt(&self, tx_hash: TxHash) -> Option<ZkReceiptEnvelope> {
        self.tx_data
            .get(&tx_hash)
            .map(|r| r.value().receipt.clone())
    }

    /// Retrieves transaction metadata by its hash, if present.
    pub fn get_transaction_meta(&self, tx_hash: TxHash) -> Option<TxMeta> {
        self.tx_data.get(&tx_hash).map(|r| r.value().meta.clone())
    }

    /// Retrieves stored transaction by its hash, if present.
    pub fn get_stored_tx_by_hash(&self, tx_hash: TxHash) -> Option<StoredTxData> {
        self.tx_data
            .get(&tx_hash)
            .map(|r| r.value().as_ref().clone())
    }

    /// Retrieves the tx data for `tx_hashes`. Returns error if any is missing.
    pub fn get_by_hashes(&self, tx_hashes: &[TxHash]) -> Option<Vec<Arc<StoredTxData>>> {
        let mut result = Vec::new();

        for tx_hash in tx_hashes {
            if let Some(data) = self.tx_data.get(tx_hash) {
                result.push(data.value().clone());
            } else {
                return None;
            }
        }

        Some(result)
    }

    pub fn get_transaction_hash_by_sender_nonce(
        &self,
        sender: Address,
        nonce: TxNonce,
    ) -> Option<TxHash> {
        self.sender_nonce_index
            .get(&(sender, nonce))
            .map(|tx_hash| *tx_hash)
    }

    pub fn remove_by_hashes(&self, tx_hashes: &[TxHash]) {
        for tx_hash in tx_hashes {
            self.tx_data.remove(tx_hash);
        }
    }

    /// Fetches the total number of transactions kept in-memory.
    pub fn len(&self) -> usize {
        self.tx_data.len()
    }
}

impl Default for TransactionReceiptRepository {
    fn default() -> Self {
        Self::new()
    }
}

fn transaction_to_api_data(
    block_output: &Sealed<BlockOutput>,
    index: usize,
    number_of_logs_before_this_tx: u64,
    cumulative_gas_used_before_this_tx: u64,
    tx: ZkTransaction,
) -> StoredTxData {
    let tx_output = block_output.tx_results[index].as_ref().ok().unwrap();

    let l2_to_l1_logs = tx_output
        .l2_to_l1_logs
        .iter()
        .map(|l2_to_l1_log| L2ToL1Log {
            l2_shard_id: l2_to_l1_log.log.l2_shard_id,
            is_service: l2_to_l1_log.log.is_service,
            tx_number_in_block: l2_to_l1_log.log.tx_number_in_block,
            sender: l2_to_l1_log.log.sender,
            key: l2_to_l1_log.log.key,
            value: l2_to_l1_log.log.value,
        })
        .collect();
    let receipt = ZkReceiptEnvelope::from_typed(
        tx.tx_type(),
        ZkReceipt {
            status: matches!(tx_output.execution_result, ExecutionResult::Success(_)).into(),
            cumulative_gas_used: cumulative_gas_used_before_this_tx + tx_output.gas_used,
            logs: tx_output.logs.clone(),
            l2_to_l1_logs,
        },
    );
    let meta = TxMeta {
        block_hash: block_output.hash(),
        block_number: block_output.header.number,
        block_timestamp: block_output.header.timestamp,
        tx_index_in_block: index as u64,
        effective_gas_price: tx
            .inner
            .inner()
            .effective_gas_price(block_output.header.base_fee_per_gas),
        number_of_logs_before_this_tx,
        gas_used: tx_output.gas_used,
        contract_address: tx_output.contract_address,
    };

    StoredTxData { tx, receipt, meta }
}
