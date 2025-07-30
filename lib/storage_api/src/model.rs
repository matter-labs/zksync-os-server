use alloy::primitives::{Address, B256};
use alloy::rlp::{RlpDecodable, RlpEncodable};
use zksync_os_types::{ZkReceiptEnvelope, ZkTransaction};

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
    pub tx: ZkTransaction,
    pub receipt: ZkReceiptEnvelope,
    pub meta: TxMeta,
}
