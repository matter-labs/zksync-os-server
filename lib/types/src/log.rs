// Copy of zk_ee::L2ToL1Log. Needed because there is no lightweight zksync-os types crate yet.

use alloy::primitives::{Address, B256};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use serde::{Deserialize, Serialize};

pub const L2_TO_L1_LOG_SERIALIZE_SIZE: usize = 88;
pub const L2_TO_L1_TREE_SIZE: usize = 16384;

///
/// L2 to l1 log structure, used for merkle tree leaves.
/// This structure holds both kinds of logs (user messages
/// and l1 -> l2 tx logs).
///
#[derive(
    Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize, RlpEncodable, RlpDecodable,
)]
pub struct L2ToL1Log {
    ///
    /// Shard id.
    /// Deprecated, kept for compatibility, always set to 0.
    ///
    pub l2_shard_id: u8,
    ///
    /// Boolean flag.
    /// Deprecated, kept for compatibility, always set to `true`.
    ///
    pub is_service: bool,
    ///
    /// The L2 transaction number in a block, in which the log was sent
    ///
    pub tx_number_in_block: u16,
    ///
    /// The L2 address which sent the log.
    /// For user messages set to `L1Messenger` system hook address,
    /// for l1 -> l2 txs logs - `BootloaderFormalAddress`.
    ///
    pub sender: Address,
    ///
    /// The 32 bytes of information that was sent in the log.
    /// For user messages used to save message sender address(padded),
    /// for l1 -> l2 txs logs - transaction hash.
    ///
    pub key: B256,
    ///
    /// The 32 bytes of information that was sent in the log.
    /// For user messages used to save message hash.
    /// for l1 -> l2 txs logs - success flag(padded).
    ///
    pub value: B256,
}

impl L2ToL1Log {
    ///
    /// Encode L2 to l1 log using solidity abi packed encoding.
    ///
    pub fn encode(&self) -> [u8; L2_TO_L1_LOG_SERIALIZE_SIZE] {
        let mut buffer = [0u8; L2_TO_L1_LOG_SERIALIZE_SIZE];
        buffer[0..1].copy_from_slice(&[self.l2_shard_id]);
        buffer[1..2].copy_from_slice(&[if self.is_service { 1 } else { 0 }]);
        buffer[2..4].copy_from_slice(&self.tx_number_in_block.to_be_bytes());
        buffer[4..24].copy_from_slice(self.sender.as_slice());
        buffer[24..56].copy_from_slice(self.key.as_slice());
        buffer[56..88].copy_from_slice(self.value.as_slice());
        buffer
    }
}
