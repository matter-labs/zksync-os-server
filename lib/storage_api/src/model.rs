use alloy::primitives::{Address, B256};
use alloy::rlp::{RlpDecodable, RlpEncodable};
use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};
use type_hash::TypeHash;
use zk_os_forward_system::run::BlockContext;
use zksync_os_types::{L1TxSerialId, ZkEnvelope, ZkReceiptEnvelope, ZkTransaction};

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

/// Used to figure out what type replay to deserialize in external node.
/// Must be incremented when [ReplayRecord] is changed.
pub const CURRENT_REPLAY_VERSION: u32 = 1;

/// Full data needed to replay a block - assuming storage is already in the correct state.
/// blah blah
///
/// When you changes this struct or any of its dependencies, you must increment [CURRENT_REPLAY_VERSION],
/// move the current struct into [OldReplayRecord] and implement conversion to the new version.
#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode, TypeHash)]
pub struct ReplayRecord {
    #[bincode(with_serde)]
    #[type_hash(foreign_type)] // TODO derive TypeHash in zksync-os
    pub block_context: BlockContext,
    /// L1 transaction serial id (0-based) expected at the beginning of this block.
    /// If `l1_transactions` is non-empty, equals to the first tx id in this block
    /// otherwise, `last_processed_l1_tx_id` equals to the previous block's value
    pub starting_l1_priority_id: L1TxSerialId,
    pub transactions: Vec<ZkTransaction>,
    /// The field is used to generate the prover input for the block in ProverInputGenerator.
    /// Will be moved to the BlockContext at some point
    pub previous_block_timestamp: u64,
    /// Version of the node that created this replay record.
    #[bincode(with_serde)]
    #[type_hash(foreign_type)]
    pub node_version: semver::Version,
    /// Hash of the block output.
    #[bincode(with_serde)]
    #[type_hash(foreign_type)]
    pub block_output_hash: B256,

    pub what_a_field: (),
}

impl ReplayRecord {
    pub fn new(
        block_context: BlockContext,
        starting_l1_priority_id: L1TxSerialId,
        transactions: Vec<ZkTransaction>,
        previous_block_timestamp: u64,
        node_version: semver::Version,
        block_output_hash: B256,
    ) -> Self {
        let first_l1_tx_priority_id = transactions.iter().find_map(|tx| match tx.envelope() {
            ZkEnvelope::L1(l1_tx) => Some(l1_tx.priority_id()),
            ZkEnvelope::L2(_) => None,
            ZkEnvelope::Upgrade(_) => None,
        });
        if let Some(first_l1_tx_priority_id) = first_l1_tx_priority_id {
            assert_eq!(
                first_l1_tx_priority_id, starting_l1_priority_id,
                "First L1 tx priority id must match next_l1_priority_id"
            );
        }
        assert!(
            !transactions.is_empty(),
            "Block must contain at least one tx"
        );
        Self {
            block_context,
            starting_l1_priority_id,
            transactions,
            previous_block_timestamp,
            node_version,
            block_output_hash,
            what_a_field: (),
        }
    }
}

#[derive(bincode::Decode)]
pub struct OldReplayRecord;

impl From<OldReplayRecord> for ReplayRecord {
    fn from(_: OldReplayRecord) -> Self {
        unimplemented!()
    }
}

/// Chain's L1 finality status. Does not track last proved block as there is no need for it (yet).
#[derive(Clone, Debug)]
pub struct FinalityStatus {
    pub last_committed_block: u64,
    pub last_executed_block: u64,
}
