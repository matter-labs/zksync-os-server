//! Conversions between storage replay records and the supported versioned wire formats.
//!
//! Unlike the versioned wire structs, these implementations may evolve with the storage API.

use crate::wire::replays::{WireReplayRecord, v0, v3};
use crate::wire::{BlockHashes, ForcedPreimage};
use alloy::consensus::crypto::RecoveryError;
use alloy::primitives::{BlockNumber, Bytes};
use zksync_os_metadata::NODE_SEMVER_VERSION;
use zksync_os_storage_api::BlockContext as StorageBlockContext;
use zksync_os_storage_api::BlockHashes as StorageBlockHashes;
use zksync_os_storage_api::ReplayRecord as StorageReplayRecord;
use zksync_os_types::{BlockStartCursors, ProtocolSemanticVersion};

// Test-only v0 replay record conversions.

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
        let block_context = StorageBlockContext {
            block_number: value.block_number,
            ..Default::default()
        };
        Ok(Self {
            block_context,
            transactions: vec![],
            previous_block_timestamp: 0,
            node_version: semver::Version::new(0, 0, 0),
            protocol_version: ProtocolSemanticVersion::new(0, 0, 0),
            block_output_hash: Default::default(),
            force_preimages: vec![],
            starting_cursors: BlockStartCursors::default(),
        })
    }
}

// Production v3 replay record conversions (`zks/5` pins this format).

impl WireReplayRecord for v3::ReplayRecord {
    fn block_number(&self) -> BlockNumber {
        self.block_context.block_number
    }
}

impl From<StorageBlockContext> for v3::BlockContext {
    fn from(value: StorageBlockContext) -> Self {
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

impl From<v3::BlockContext> for StorageBlockContext {
    fn from(value: v3::BlockContext) -> Self {
        Self {
            chain_id: value.chain_id,
            block_number: value.block_number,
            block_hashes: StorageBlockHashes(value.block_hashes.0),
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

impl From<StorageReplayRecord> for v3::ReplayRecord {
    fn from(value: StorageReplayRecord) -> Self {
        Self {
            block_context: value.block_context.into(),
            starting_l1_priority_id: value.starting_cursors.l1_priority_id,
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
            starting_interop_root_id: value.starting_cursors.interop_root_id,
            starting_migration_number: value.starting_cursors.migration_number,
            starting_interop_fee_number: value.starting_cursors.interop_fee_number,
        }
    }
}

impl TryFrom<v3::ReplayRecord> for StorageReplayRecord {
    type Error = RecoveryError;

    fn try_from(value: v3::ReplayRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            block_context: StorageBlockContext::from(value.block_context),
            transactions: value
                .transactions
                .into_iter()
                .map(|tx| tx.try_into_recovered())
                .collect::<Result<Vec<_>, _>>()?,
            previous_block_timestamp: value.previous_block_timestamp,
            // Replay wire formats omit node semver, so the receiver stamps its own binary version.
            node_version: NODE_SEMVER_VERSION.clone(),
            protocol_version: value.protocol_version,
            block_output_hash: value.block_output_hash,
            force_preimages: value
                .force_preimages
                .into_iter()
                .map(|p| (p.hash, p.preimage.into()))
                .collect(),
            starting_cursors: BlockStartCursors {
                l1_priority_id: value.starting_l1_priority_id,
                interop_root_id: value.starting_interop_root_id,
                migration_number: value.starting_migration_number,
                interop_fee_number: value.starting_interop_fee_number,
            },
        })
    }
}
