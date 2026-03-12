use alloy::primitives::{Address, B256};
use alloy::rlp::{RlpDecodable, RlpEncodable};
use serde::{Deserialize, Serialize};
use zksync_os_interface::types::BlockContext;
use zksync_os_types::{
    InteropRootsLogIndex, L1TxSerialId, ProtocolSemanticVersion, ZkEnvelope, ZkReceiptEnvelope,
    ZkTransaction,
};

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

/// Full data needed to replay a block - assuming storage is already in the correct state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayRecord {
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
    /// NOTE: Excluded from equality check, different node versions can produce identical blocks.
    pub node_version: semver::Version,
    /// Version of the protocol that was used to create this replay record.
    pub protocol_version: ProtocolSemanticVersion,
    /// Extension of traditional block hash (see hash_block_output)
    /// Used to confirm that we executed the replay correctly
    /// We need this because our header is missing a few important fields
    // TODO: We may want to add more information about block_output_hash (currently tracks only output, but different input can result in same output)
    pub block_output_hash: B256,
    /// Forced preimages to be included before the block execution.
    pub force_preimages: Vec<(B256, Vec<u8>)>,
    /// Event index(block number and index in block) of the interop root tx executed first in the block
    /// If there is no interop root tx in the block, equals to the previous block's value
    pub starting_interop_event_index: InteropRootsLogIndex,
    /// Migration number at the beginning of the block. If there is no migration event in the block, equals to the previous block's value
    pub starting_migration_number: u64,
    /// Interop fee update number at the beginning of the block. If there is no interop fee update
    /// in the block, equals to the previous block's value.
    pub starting_interop_fee_number: u64,
}

impl PartialEq for ReplayRecord {
    fn eq(&self, other: &Self) -> bool {
        // Same as #[derive(PartialEq)], but without `node_version`.
        // During replay, we care about block identity, node_version is binary metadata.
        self.block_context == other.block_context
            && self.starting_l1_priority_id == other.starting_l1_priority_id
            && self.transactions == other.transactions
            && self.previous_block_timestamp == other.previous_block_timestamp
            && self.protocol_version == other.protocol_version
            && self.block_output_hash == other.block_output_hash
            && self.force_preimages == other.force_preimages
            && self.starting_interop_event_index == other.starting_interop_event_index
            && self.starting_migration_number == other.starting_migration_number
            && self.starting_interop_fee_number == other.starting_interop_fee_number
    }
}

impl ReplayRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        block_context: BlockContext,
        starting_l1_priority_id: L1TxSerialId,
        transactions: Vec<ZkTransaction>,
        previous_block_timestamp: u64,
        node_version: semver::Version,
        protocol_version: ProtocolSemanticVersion,
        block_output_hash: B256,
        force_preimages: Vec<(B256, Vec<u8>)>,
        starting_interop_event_index: InteropRootsLogIndex,
        starting_migration_number: u64,
        starting_interop_fee_number: u64,
    ) -> Self {
        let first_l1_tx_priority_id = transactions.iter().find_map(|tx| match tx.envelope() {
            ZkEnvelope::L1(l1_tx) => Some(l1_tx.priority_id()),
            _ => None,
        });
        if let Some(first_l1_tx_priority_id) = first_l1_tx_priority_id {
            assert_eq!(
                first_l1_tx_priority_id, starting_l1_priority_id,
                "First L1 tx priority id must match next_l1_priority_id"
            );
        }

        Self {
            block_context,
            starting_l1_priority_id,
            transactions,
            previous_block_timestamp,
            node_version,
            protocol_version,
            block_output_hash,
            force_preimages,
            starting_interop_event_index,
            starting_migration_number,
            starting_interop_fee_number,
        }
    }
}

/// Chain's L1 finality status. Does not track last proved block as there is no need for it (yet).
#[derive(Clone, Debug)]
pub struct FinalityStatus {
    pub last_committed_block: u64,
    pub last_committed_batch: u64,
    pub last_executed_block: u64,
    pub last_executed_batch: u64,
}
