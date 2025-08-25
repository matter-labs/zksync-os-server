use crate::{ZkEnvelope, ZkReceiptEnvelope};
use alloy::primitives::B256;
use alloy::rpc::types::Log;
use serde::{Deserialize, Serialize};

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

pub type ZkTransaction = alloy::rpc::types::Transaction<ZkEnvelope>;
pub type ZkTransactionReceipt = alloy::rpc::types::TransactionReceipt<ZkReceiptEnvelope<Log>>;
pub type ZkBlock = alloy::rpc::types::Block<ZkTransaction>;
pub type ZkHeader = alloy::rpc::types::Header;
