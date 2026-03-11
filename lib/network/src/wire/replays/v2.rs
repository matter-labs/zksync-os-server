//! We need to not accidentally change the replay wire format but there is no way in Rust to get a
//! stable unique ID for a type, so instead we define it in this separate file.
//!
//! Do not change this file under any circumstances. Copy it instead. May be deleted when obsolete.

// Difference from v1:
// - Added `starting_migration_number` field to `ReplayRecord`.
// - Added `starting_interop_fee_number` field to `ReplayRecord`.

use crate::wire::{BlockHashes, ForcedPreimage};
use alloy::primitives::{Address, B256, U256};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use zksync_os_types::{InteropRootsLogIndex, L1TxSerialId, ProtocolSemanticVersion, ZkEnvelope};

#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
pub struct ReplayRecord {
    pub block_context: BlockContext,
    pub starting_l1_priority_id: L1TxSerialId,
    pub transactions: Vec<ZkEnvelope>,
    pub previous_block_timestamp: u64,
    pub protocol_version: ProtocolSemanticVersion,
    pub block_output_hash: B256,
    pub force_preimages: Vec<ForcedPreimage>,
    pub starting_interop_event_index: InteropRootsLogIndex,
    pub starting_migration_number: u64,
    pub starting_interop_fee_number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
pub struct BlockContext {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hashes: BlockHashes,
    pub timestamp: u64,
    pub eip1559_basefee: U256,
    pub pubdata_price: U256,
    pub native_price: U256,
    pub coinbase: Address,
    pub gas_limit: u64,
    pub pubdata_limit: u64,
    pub mix_hash: U256,
    pub execution_version: u32,
    pub blob_fee: U256,
}
