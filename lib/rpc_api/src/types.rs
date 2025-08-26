use alloy::consensus::{Sealed, TransactionEnvelope};
use alloy::network::primitives::BlockTransactions;
use alloy::primitives::{B256, TxHash, U256};
use alloy::rpc::types::Log;
use jsonrpsee::core::Serialize;
use serde::Deserialize;
use zksync_os_types::{
    BlockExt, L1PriorityEnvelope, L1UpgradeEnvelope, L2Envelope, ZkEnvelope, ZkReceiptEnvelope,
};

pub type ZkTransactionReceipt = alloy::rpc::types::TransactionReceipt<ZkReceiptEnvelope<Log>>;
pub type ZkHeader = alloy::rpc::types::Header;

/// Duplicate of `ZkEnvelope` with proper `ty` for Upgrade and L1Priority transactions.
/// This type is not rlp-encodable/decodable, it must only be used for RPC serialization.
#[derive(Debug, Clone, TransactionEnvelope)]
#[envelope(alloy_consensus = alloy::consensus, tx_type_name = ZkApiTxType)]
pub enum ZkApiEnvelope {
    #[envelope(flatten)]
    L2(L2Envelope),
    #[envelope(ty = 254)]
    Upgrade(L1UpgradeEnvelope),
    #[envelope(ty = 255)]
    L1(L1PriorityEnvelope),
}

pub type ZkApiTransaction = alloy::rpc::types::Transaction<ZkApiEnvelope>;

pub fn convert_envelope_to_api(e: ZkEnvelope) -> ZkApiEnvelope {
    match e {
        ZkEnvelope::L2(e) => ZkApiEnvelope::L2(e),
        ZkEnvelope::Upgrade(e) => ZkApiEnvelope::Upgrade(e),
        ZkEnvelope::L1(e) => ZkApiEnvelope::L1(e),
    }
}

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
            ZkHeader::from_consensus(block.header.seal(hash), None, Some(U256::from(rlp_length))),
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
