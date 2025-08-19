use crate::{BlockExt, ZkEnvelope, ZkReceiptEnvelope};
use alloy::consensus::Sealed;
use alloy::network::primitives::BlockTransactions;
use alloy::primitives::{B256, TxHash, U256};
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

pub trait RpcBlockConvert {
    fn into_rpc(self) -> ZkBlock;
}

impl RpcBlockConvert for Sealed<alloy::consensus::Block<TxHash>> {
    fn into_rpc(self) -> ZkBlock {
        let hash = self.hash();
        let block = self.unseal();
        let rlp_length = block.rlp_length();
        ZkBlock::new(
            ZkHeader::from_consensus(block.header.seal(hash), None, Some(U256::from(rlp_length))),
            BlockTransactions::Hashes(block.body.transactions),
        )
    }
}
