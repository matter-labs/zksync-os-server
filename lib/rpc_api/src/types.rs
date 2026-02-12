use alloy::consensus::Sealed;
use alloy::network::primitives::BlockTransactions;
use alloy::primitives::{Address, B256, BlockHash, TxHash, U256};
use alloy::rpc::types::Log;
use jsonrpsee::core::Serialize;
use serde::Deserialize;
use zksync_os_types::{BlockExt, ZkEnvelope, ZkReceiptEnvelope};

pub type ZkTransactionReceipt =
    alloy::rpc::types::TransactionReceipt<ZkReceiptEnvelope<Log, L2ToL1Log>>;
pub type ZkHeader = alloy::rpc::types::Header;

pub type ZkApiTransaction = alloy::rpc::types::Transaction<ZkEnvelope>;

pub type ZkApiBlock = alloy::rpc::types::Block<ZkApiTransaction>;

pub trait RpcBlockConvert {
    fn into_rpc(self) -> ZkApiBlock;
}

impl RpcBlockConvert for Sealed<alloy::consensus::Block<TxHash>> {
    fn into_rpc(self) -> ZkApiBlock {
        let hash = self.hash();
        let block = self.unseal();
        let rlp_length = block.rlp_length();
        ZkApiBlock::new(
            ZkHeader::from_consensus(
                block.header.seal(hash),
                Some(U256::ZERO),
                Some(U256::from(rlp_length)),
            ),
            BlockTransactions::Hashes(block.body.transactions),
        )
    }
}

/// A struct with the proof for the L2->L1 log in a specific block.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct L2ToL1LogProof {
    /// The L1 batch number containing the log.
    pub batch_number: u64,
    /// The merkle path for the leaf.
    pub proof: Vec<B256>,
    /// The id of the leaf in a tree.
    pub id: u32,
    /// The root of the tree.
    pub root: B256,
}

/// ZKsync-specific block metadata struct.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BlockMetadata {
    pub pubdata_price_per_byte: U256,
    pub native_price: U256,
    pub execution_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L2ToL1Log {
    /// Hash of the block the transaction that emitted this log was mined in
    pub block_hash: Option<BlockHash>,
    /// Number of the block the transaction that emitted this log was mined in
    #[serde(with = "alloy::serde::quantity::opt")]
    pub block_number: Option<u64>,
    /// The timestamp of the block.
    #[serde(with = "alloy::serde::quantity::opt")]
    pub block_timestamp: Option<u64>,
    /// Transaction Hash
    #[doc(alias = "tx_hash")]
    pub transaction_hash: Option<TxHash>,
    /// Index of the Transaction in the block
    #[serde(with = "alloy::serde::quantity::opt")]
    #[doc(alias = "tx_index")]
    pub transaction_index: Option<u64>,
    /// Log Index in Block
    #[serde(with = "alloy::serde::quantity::opt")]
    pub log_index: Option<u64>,
    /// Log Index in Transaction, needed for compatibility with ZKSync Era L2->L1 log format.
    #[serde(with = "alloy::serde::quantity::opt")]
    pub transaction_log_index: Option<u64>,
    /// Deprecated, kept for compatibility, always set to 0.
    #[serde(with = "alloy::serde::quantity")]
    pub shard_id: u64,
    /// Deprecated, kept for compatibility, always set to `true`.
    pub is_service: bool,
    /// The L2 address which sent the log.
    /// For user messages set to `L1Messenger` system hook address,
    /// for l1 -> l2 txs logs - `BootloaderFormalAddress`.
    pub sender: Address,
    /// The 32 bytes of information that was sent in the log.
    /// For user messages used to save message sender address(padded),
    /// for l1 -> l2 txs logs - transaction hash.
    pub key: B256,
    /// The 32 bytes of information that was sent in the log.
    /// For user messages used to save message hash.
    /// for l1 -> l2 txs logs - success flag(padded).
    pub value: B256,
}

impl From<L2ToL1Log> for zksync_os_types::L2ToL1Log {
    fn from(value: L2ToL1Log) -> Self {
        Self {
            l2_shard_id: value.shard_id as u8,
            is_service: value.is_service,
            tx_number_in_block: value.transaction_index.expect("Missing transaction index") as u16,
            sender: value.sender,
            key: value.key,
            value: value.value,
        }
    }
}
