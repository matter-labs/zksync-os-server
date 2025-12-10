//! Implements the `GetBlockReplays`, and `BlockReplays` message types.

mod v1;

use crate::wire::{BlockHashes, ForcedPreimage};
use alloy::primitives::{BlockNumber, Bytes};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use serde::{Deserialize, Serialize};
use zksync_os_interface::types::BlockContext as InterfaceBlockContext;
use zksync_os_storage_api::ReplayRecord as StorageReplayRecord;

/// A request for a peer to return block replays starting at the requested block number.
/// The peer MUST start streaming indefinite number of [`BlockReplays`] responses.
#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable, Serialize, Deserialize)]
pub struct GetBlockReplays {
    /// The block number that the peer should start returning replay blocks from.
    pub starting_block: u64,
    /// Records for which DB keys should be overridden. Used only for debugging.
    pub record_overrides: Vec<RecordOverride>,
}

/// Specifies one overridden block replay record. This allows EN to sync replay record that is not
/// a part of the canonical chain (useful for debugging reverted blocks).
#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable, Serialize, Deserialize)]
pub struct RecordOverride {
    /// Block number for which record should be pulled from a different DB key.
    pub block_number: BlockNumber,
    /// DB key to use when reading replay record.
    pub db_key: Bytes,
}

/// The response to [`GetBlockReplays`], containing one or more consecutive replay records.
// todo: consider making this <T> over possible versions of `ReplayRecord`
#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable, Serialize, Deserialize)]
pub struct BlockReplays {
    pub records: Vec<v1::ReplayRecord>,
}

impl BlockReplays {
    pub fn new(records: Vec<StorageReplayRecord>) -> Self {
        let records = records.into_iter().map(v1::ReplayRecord::from).collect();
        Self { records }
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
        }
    }
}
