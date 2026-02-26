//! Trait implementations for all versioned replay messages.
//!
//! Note that this file is allowed to change as traits can evolve over time and hence can the
//! surrounding logic.

use crate::wire::replays::{WireReplayRecord, v0, v1};
use crate::wire::{BlockHashes, ForcedPreimage};
use alloy::consensus::crypto::RecoveryError;
use alloy::primitives::{BlockNumber, Bytes};
use zksync_os_interface::types::BlockContext as InterfaceBlockContext;
use zksync_os_interface::types::BlockHashes as InterfaceBlockHashes;
use zksync_os_metadata::NODE_SEMVER_VERSION;
use zksync_os_storage_api::ReplayRecord as StorageReplayRecord;
use zksync_os_types::{InteropRootsLogIndex, ProtocolSemanticVersion};

// ==================================================
// | Implementations for protocol version 0 (Dummy) |
// ==================================================

impl WireReplayRecord for v0::ReplayRecord {
    fn block_number(&self) -> BlockNumber {
        self.block_number
    }
}

impl From<StorageReplayRecord> for v0::ReplayRecord {
    fn from(value: StorageReplayRecord) -> Self {
        Self {
            block_number: value.block_context.block_number,
        }
    }
}

impl TryFrom<v0::ReplayRecord> for StorageReplayRecord {
    type Error = RecoveryError;

    fn try_from(value: v0::ReplayRecord) -> Result<Self, Self::Error> {
        let block_context = InterfaceBlockContext {
            block_number: value.block_number,
            ..Default::default()
        };
        Ok(Self {
            block_context,
            starting_l1_priority_id: 0,
            transactions: vec![],
            previous_block_timestamp: 0,
            node_version: semver::Version::new(0, 0, 0),
            protocol_version: ProtocolSemanticVersion::new(0, 0, 0),
            block_output_hash: Default::default(),
            force_preimages: vec![],
            starting_interop_event_index: InteropRootsLogIndex::default(),
        })
    }
}

// ==========================================
// | Implementations for protocol version 1 |
// ==========================================

impl WireReplayRecord for v1::ReplayRecord {
    fn block_number(&self) -> BlockNumber {
        self.block_context.block_number
    }
}

impl From<InterfaceBlockContext> for v1::BlockContext {
    fn from(value: InterfaceBlockContext) -> Self {
        Self {
            chain_id: value.chain_id,
            block_number: value.block_number,
            block_hashes: BlockHashes(value.block_hashes.0),
            timestamp: value.timestamp,
            eip1559_basefee: value.eip1559_basefee,
            pubdata_price: value.pubdata_price,
            native_price: value.native_price,
            coinbase: value.coinbase,
            gas_limit: value.gas_limit,
            pubdata_limit: value.pubdata_limit,
            mix_hash: value.mix_hash,
            execution_version: value.execution_version,
            blob_fee: value.blob_fee,
        }
    }
}

impl From<StorageReplayRecord> for v1::ReplayRecord {
    fn from(value: StorageReplayRecord) -> Self {
        Self {
            block_context: value.block_context.into(),
            starting_l1_priority_id: value.starting_l1_priority_id,
            transactions: value
                .transactions
                .into_iter()
                .map(|tx| tx.into_envelope())
                .collect(),
            previous_block_timestamp: value.previous_block_timestamp,
            protocol_version: value.protocol_version,
            block_output_hash: value.block_output_hash,
            force_preimages: value
                .force_preimages
                .into_iter()
                .map(|(hash, preimage)| ForcedPreimage {
                    hash,
                    preimage: Bytes::from(preimage),
                })
                .collect(),
            starting_interop_event_index: value.starting_interop_event_index,
        }
    }
}

impl From<v1::BlockContext> for InterfaceBlockContext {
    fn from(value: v1::BlockContext) -> Self {
        Self {
            chain_id: value.chain_id,
            block_number: value.block_number,
            block_hashes: InterfaceBlockHashes(value.block_hashes.0),
            timestamp: value.timestamp,
            eip1559_basefee: value.eip1559_basefee,
            pubdata_price: value.pubdata_price,
            native_price: value.native_price,
            coinbase: value.coinbase,
            gas_limit: value.gas_limit,
            pubdata_limit: value.pubdata_limit,
            mix_hash: value.mix_hash,
            execution_version: value.execution_version,
            blob_fee: value.blob_fee,
            code_size_limit: None,
        }
    }
}

impl TryFrom<v1::ReplayRecord> for StorageReplayRecord {
    type Error = RecoveryError;

    fn try_from(value: v1::ReplayRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            block_context: value.block_context.into(),
            starting_l1_priority_id: value.starting_l1_priority_id,
            transactions: value
                .transactions
                .into_iter()
                .map(|tx| tx.try_into_recovered())
                .collect::<Result<Vec<_>, _>>()?,
            previous_block_timestamp: value.previous_block_timestamp,
            // Stamp replay record with this node's version
            node_version: NODE_SEMVER_VERSION.clone(),
            protocol_version: value.protocol_version,
            block_output_hash: value.block_output_hash,
            force_preimages: value
                .force_preimages
                .into_iter()
                .map(|p| (p.hash, p.preimage.into()))
                .collect(),
            starting_interop_event_index: value.starting_interop_event_index,
        })
    }
}
