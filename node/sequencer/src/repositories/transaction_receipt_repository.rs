use alloy::consensus::transaction::TransactionInfo;
use alloy::consensus::{Receipt, ReceiptEnvelope, ReceiptWithBloom, TxType};
use alloy::primitives::{Address, LogData, Sealed, B256};
use dashmap::DashMap;
use std::sync::Arc;
use zk_ee::utils::Bytes32;
use zk_os_forward_system::run::{BatchOutput, ExecutionResult};
use zksync_os_types::{ZkEnvelope, ZkTransaction, ZkTxType};

#[derive(Clone, Debug)]
pub struct TransactionApiData {
    pub transaction: alloy::rpc::types::Transaction<ZkEnvelope>,
    pub receipt: alloy::rpc::types::TransactionReceipt,
}

/// Thread-safe in-memory repository of transaction receipts, keyed by transaction hash.
///
/// Retains all inserted receipts indefinitely. Internally uses a lock-free
/// DashMap to allow concurrent inserts and lookups.
///
/// todo: unbounded memory use
#[derive(Clone, Debug)]
pub struct TransactionReceiptRepository {
    /// Map from tx hash → receipt data
    receipts: Arc<DashMap<Bytes32, TransactionApiData>>,
}

impl TransactionReceiptRepository {
    /// Creates a new repository.
    pub fn new() -> Self {
        TransactionReceiptRepository {
            receipts: Arc::new(DashMap::new()),
        }
    }

    /// Inserts a receipt for `tx_hash`. If a receipt for the same hash
    /// already exists, it will be overwritten.
    pub fn insert(&self, tx_hash: Bytes32, data: TransactionApiData) {
        self.receipts.insert(tx_hash, data);
    }

    /// Retrieves the receipt for `tx_hash`, if present.
    /// Returns a cloned `TransactionApiData`.
    pub fn get_by_hash(&self, tx_hash: &Bytes32) -> Option<TransactionApiData> {
        self.receipts.get(tx_hash).map(|r| r.value().clone())
    }

    /// Returns `true` if a receipt for `tx_hash` is present.
    pub fn contains(&self, tx_hash: &Bytes32) -> bool {
        self.receipts.contains_key(tx_hash)
    }
}

impl Default for TransactionReceiptRepository {
    fn default() -> Self {
        Self::new()
    }
}

pub fn transaction_to_api_data(
    block_output: &Sealed<BatchOutput>,
    index: usize,
    log_index: usize,
    tx: ZkTransaction,
) -> TransactionApiData {
    let tx_hash = *tx.hash();
    let tx_output = block_output.tx_results[index].as_ref().ok().unwrap();

    let logs = tx_output
        .logs
        .iter()
        .enumerate()
        .map(|(i, log)| {
            let inner = alloy::primitives::Log {
                address: Address::from(log.address.to_be_bytes()),
                data: LogData::new(
                    log.topics
                        .iter()
                        .map(|topic| B256::from(topic.as_u8_array()))
                        .collect(),
                    log.data.clone().into(),
                )
                .unwrap(),
            };
            alloy::rpc::types::Log {
                inner,
                block_hash: Some(block_output.hash()),
                block_number: Some(block_output.header.number),
                block_timestamp: Some(block_output.header.timestamp),
                transaction_hash: Some(tx_hash),
                transaction_index: Some(index as u64),
                log_index: Some((log_index + i) as u64),
                removed: false,
            }
        })
        .collect::<Vec<_>>();
    let tx_receipt = Receipt {
        status: matches!(tx_output.execution_result, ExecutionResult::Success(_)).into(),
        // todo
        cumulative_gas_used: 7777,
        logs,
    };
    let logs_bloom = tx_receipt.bloom_slow();
    let receipt_with_bloom = ReceiptWithBloom::new(tx_receipt, logs_bloom);
    let receipt_envelope = match tx.tx_type() {
        // TODO: For now, pretend like L1 transactions are legacy for the purposes of API
        //       Needs to be changed when we add L1-specific receipt type
        ZkTxType::L1 => ReceiptEnvelope::Legacy(receipt_with_bloom),
        ZkTxType::L2(TxType::Legacy) => ReceiptEnvelope::Legacy(receipt_with_bloom),
        ZkTxType::L2(TxType::Eip2930) => ReceiptEnvelope::Eip2930(receipt_with_bloom),
        ZkTxType::L2(TxType::Eip1559) => ReceiptEnvelope::Eip1559(receipt_with_bloom),
        ZkTxType::L2(TxType::Eip4844) => ReceiptEnvelope::Eip4844(receipt_with_bloom),
        ZkTxType::L2(TxType::Eip7702) => ReceiptEnvelope::Eip7702(receipt_with_bloom),
    };
    let rpc_receipt = alloy::rpc::types::TransactionReceipt {
        inner: receipt_envelope,
        transaction_hash: tx_hash,
        transaction_index: Some(index as u64),
        block_hash: Some(block_output.hash()),
        block_number: Some(block_output.header.number),
        gas_used: tx_output.gas_used,
        effective_gas_price: block_output.header.base_fee_per_gas as u128,
        blob_gas_used: None,
        blob_gas_price: None,
        from: tx.signer(),
        to: tx.to(),
        contract_address: tx_output
            .contract_address
            .map(|c| Address::from(c.to_be_bytes())),
    };

    let rpc_transaction = alloy::rpc::types::Transaction::from_transaction(
        tx.inner,
        TransactionInfo {
            hash: Some(tx_hash),
            index: Some(index as u64),
            block_hash: Some(block_output.hash()),
            block_number: Some(block_output.header.number),
            base_fee: Some(block_output.header.base_fee_per_gas),
        },
    );

    TransactionApiData {
        transaction: rpc_transaction,
        receipt: rpc_receipt,
    }
}
