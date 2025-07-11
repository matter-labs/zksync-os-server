use crate::CHAIN_ID;
use alloy::consensus::{Receipt, ReceiptEnvelope, ReceiptWithBloom, Signed, TxLegacy, TxType};
use alloy::primitives::{Address, Log, LogData, TxHash, TxKind, B256, U256};
use alloy::signers::Signature;
use alloy_rlp::{RlpDecodable, RlpEncodable};
use dashmap::DashMap;
use reth_primitives::Recovered;
use std::sync::Arc;
use zk_os_forward_system::run::{BatchOutput, ExecutionResult};
use zksync_os_types::{L1Transaction, L2Envelope, L2Transaction};

#[derive(Debug, Clone, RlpEncodable, RlpDecodable)]
#[rlp(trailing)]
pub struct TxMeta {
    pub block_hash: B256,
    pub block_number: u64,
    pub block_timestamp: u64,
    pub tx_index_in_block: u64,
    pub effective_gas_price: u128,
    pub number_of_logs_before_this_tx: u64,
    pub gas_used: u64,
    pub contract_address: Option<Address>,
}

#[derive(Debug, Clone)]
pub struct StoredTxData {
    pub tx: L2Transaction,
    pub receipt: ReceiptEnvelope,
    pub meta: TxMeta,
}

/// Thread-safe in-memory repository of transaction receipts, keyed by transaction hash.
///
/// Retains all inserted receipts indefinitely. Internally uses a lock-free
/// DashMap to allow concurrent inserts and lookups.
///
/// todo: unbounded memory use
#[derive(Clone, Debug)]
pub struct TransactionReceiptRepository {
    /// Map from tx hash → (tx, receipt).
    tx_data: Arc<DashMap<TxHash, StoredTxData>>,
}

impl TransactionReceiptRepository {
    /// Creates a new repository.
    pub fn new() -> Self {
        TransactionReceiptRepository {
            tx_data: Arc::new(DashMap::new()),
        }
    }

    /// Inserts data for `tx_hash`. If a data for the same hash
    /// already exists, it will be overwritten.
    pub fn insert(&self, tx_hash: TxHash, data: StoredTxData) {
        self.tx_data.insert(tx_hash, data);
    }

    /// Retrieves the transaction for `tx_hash`, if present.
    pub fn get_tx_by_hash(&self, tx_hash: TxHash) -> Option<L2Transaction> {
        self.tx_data.get(&tx_hash).map(|r| r.value().tx.clone())
    }

    /// Retrieves the receipt for `tx_hash`, if present.
    pub fn get_receipt_by_hash(&self, tx_hash: TxHash) -> Option<ReceiptEnvelope> {
        self.tx_data
            .get(&tx_hash)
            .map(|r| r.value().receipt.clone())
    }

    /// Retrieves add stored data for `tx_hash`, if present.
    pub fn get_stored_tx_by_hash(&self, tx_hash: TxHash) -> Option<StoredTxData> {
        self.tx_data.get(&tx_hash).map(|r| r.value().clone())
    }

    /// Retrieves the tx data for `tx_hashes`. Panics if any is missing.
    pub fn get_by_hashes(&self, tx_hashes: &[TxHash]) -> Vec<StoredTxData> {
        tx_hashes
            .iter()
            .map(|tx_hash| {
                self.tx_data
                    .get(tx_hash)
                    .map(|r| r.value().clone())
                    .unwrap_or_else(|| {
                        panic!("Missing receipt for transaction hash: {:?}", tx_hash)
                    })
            })
            .collect()
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
}

impl Default for TransactionReceiptRepository {
    fn default() -> Self {
        Self::new()
    }
}

pub fn l1_transaction_to_api_data(
    block_output: &BatchOutput,
    index: usize,
    number_of_logs_before_this_tx: u64,
    tx: L1Transaction,
) -> StoredTxData {
    let signer = Address::from(tx.common_data.sender.0);
    let to = tx.execute.contract_address.map(|c| Address::from(c.0));
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

    let tx_receipt = Receipt {
        status: matches!(tx_output.execution_result, ExecutionResult::Success(_)).into(),
        // todo
        cumulative_gas_used: 7777,
        logs,
    };
    let logs_bloom = tx_receipt.bloom_slow();
    let receipt_with_bloom = ReceiptWithBloom::new(tx_receipt, logs_bloom);
    // TODO: For now, pretend like L1 transactions are legacy for the purposes of API
    //       Needs to be changed when we migrate L1 transactions to alloy
    let receipt_envelope = ReceiptEnvelope::Legacy(receipt_with_bloom);

    let transaction = Recovered::new_unchecked(
        L2Envelope::Legacy(Signed::new_unchecked(
            TxLegacy {
                chain_id: Some(CHAIN_ID),
                nonce: tx.common_data.serial_id.0,
                gas_price: 0,
                gas_limit: tx.common_data.gas_limit.as_u64(),
                to: TxKind::Call(to.unwrap_or_default()),
                value: Default::default(),
                input: Default::default(),
            },
            Signature::new(U256::ZERO, U256::ZERO, false),
            TxHash::from(tx.hash().0),
        )),
        signer,
    );
    let meta = TxMeta {
        block_hash: B256::from(block_output.header.hash()),
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

    StoredTxData {
        tx: transaction,
        receipt: receipt_envelope,
        meta,
    }
}

pub fn l2_transaction_to_api_data(
    block_output: &BatchOutput,
    index: usize,
    number_of_logs_before_this_tx: u64,
    tx: L2Transaction,
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
    let tx_receipt = Receipt {
        status: matches!(tx_output.execution_result, ExecutionResult::Success(_)).into(),
        // todo
        cumulative_gas_used: 7777,
        logs,
    };
    let logs_bloom = tx_receipt.bloom_slow();
    let receipt_with_bloom = ReceiptWithBloom::new(tx_receipt, logs_bloom);
    let receipt_envelope = match tx.tx_type() {
        TxType::Legacy => ReceiptEnvelope::Legacy(receipt_with_bloom),
        TxType::Eip2930 => ReceiptEnvelope::Eip2930(receipt_with_bloom),
        TxType::Eip1559 => ReceiptEnvelope::Eip1559(receipt_with_bloom),
        TxType::Eip4844 => ReceiptEnvelope::Eip4844(receipt_with_bloom),
        TxType::Eip7702 => ReceiptEnvelope::Eip7702(receipt_with_bloom),
    };
    let meta = TxMeta {
        block_hash: B256::from(block_output.header.hash()),
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

    StoredTxData {
        tx,
        receipt: receipt_envelope,
        meta,
    }
}
