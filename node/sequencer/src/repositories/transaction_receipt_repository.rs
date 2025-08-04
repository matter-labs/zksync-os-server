use alloy::primitives::{Address, B256, Log, LogData, Sealed, TxHash, TxNonce};
use dashmap::DashMap;
use std::sync::Arc;
use zk_os_forward_system::run::{BlockOutput, ExecutionResult};
use zksync_os_storage_api::{StoredTxData, TxMeta};
use zksync_os_types::{L2ToL1Log, ZkReceipt, ZkReceiptEnvelope, ZkTransaction};

/// Thread-safe in-memory repository of transaction receipts, keyed by transaction hash.
///
/// Retains all inserted receipts indefinitely. Internally uses a lock-free
/// DashMap to allow concurrent inserts and lookups.
///
/// todo: unbounded memory use
#[derive(Clone, Debug)]
pub struct TransactionReceiptRepository {
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

    /// Returns `true` if a receipt for `tx_hash` is present.
    pub fn contains(&self, tx_hash: TxHash) -> bool {
        self.tx_data.contains_key(&tx_hash)
    }

    /// Fetches the total number of transactions kept in-memory.
    pub fn len(&self) -> usize {
        self.tx_data.len()
    }

    /// Check if the transaction repository is empty or not.
    pub fn is_empty(&self) -> bool {
        self.tx_data.is_empty()
    }
}

impl Default for TransactionReceiptRepository {
    fn default() -> Self {
        Self::new()
    }
}

pub fn transaction_to_api_data(
    block_output: &Sealed<BlockOutput>,
    index: usize,
    number_of_logs_before_this_tx: u64,
    tx: ZkTransaction,
) -> StoredTxData {
    let tx_output = block_output.tx_results[index].as_ref().ok().unwrap();

    let logs = tx_output
        .logs
        .iter()
        .map(|log| Log {
            address: Address::from(log.address.to_be_bytes()),
            data: LogData::new(
                log.topics
                    .iter()
                    .map(|topic| B256::from(topic.as_u8_array()))
                    .collect(),
                log.data.clone().into(),
            )
            .unwrap(),
        })
        .collect::<Vec<_>>();
    let l2_to_l1_logs = tx_output
        .l2_to_l1_logs
        .iter()
        .map(|l2_to_l1_log| L2ToL1Log {
            sender: Address::new(l2_to_l1_log.log.sender.to_be_bytes()),
            key: B256::new(l2_to_l1_log.log.key.as_u8_array()),
            value: B256::new(l2_to_l1_log.log.value.as_u8_array()),
        })
        .collect();
    let receipt = ZkReceiptEnvelope::from_typed(
        tx.tx_type(),
        ZkReceipt {
            status: matches!(tx_output.execution_result, ExecutionResult::Success(_)).into(),
            // todo
            cumulative_gas_used: 7777,
            logs,
            l2_to_l1_logs,
        },
    );
    let meta = TxMeta {
        block_hash: B256::from(block_output.hash()),
        block_number: block_output.header.number,
        block_timestamp: block_output.header.timestamp,
        tx_index_in_block: index as u64,
        effective_gas_price: block_output.header.base_fee_per_gas as u128,
        number_of_logs_before_this_tx,
        gas_used: tx_output.gas_used,
        contract_address: tx_output
            .contract_address
            .map(|a| Address::new(a.to_be_bytes())),
    };

    StoredTxData { tx, receipt, meta }
}
