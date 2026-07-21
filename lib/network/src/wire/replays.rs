//! Implements the `GetBlockReplays` and `BlockReplays` message types.
//!
//! `BlockReplays` is versioned over all the possible replay record wire formats
//! supported by this node. The formats themselves (and the [`WireReplayRecord`]
//! trait) live in `zksync_os_wire` — they are durable encodings shared with
//! consensus, not protocol messages — and are re-exported here so this crate's
//! consumers keep one import path.

pub use zksync_os_wire::replays::*;

use alloy::primitives::{BlockNumber, Bytes};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use zksync_os_storage_api::ReplayRecord as StorageReplayRecord;

/// A request for a peer to return block replays starting at the requested block number.
/// The peer MUST start streaming indefinite number of [`BlockReplays`] responses.
#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
#[rlp(trailing)]
pub struct GetBlockReplays {
    /// The block number that the peer should start returning replay blocks from.
    pub starting_block: u64,
    /// Records for which DB keys should be overridden. Used only for debugging.
    pub record_overrides: Vec<RecordOverride>,
    /// Maximum number of consecutive replay records to include in each response message.
    pub max_blocks_per_message: Option<u64>,
}

/// Specifies one overridden block replay record. This allows EN to sync replay record that is not
/// a part of the canonical chain (useful for debugging reverted blocks).
#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
pub struct RecordOverride {
    /// Block number for which record should be pulled from a different DB key.
    pub block_number: BlockNumber,
    /// DB key to use when reading replay record.
    pub db_key: Bytes,
}

/// The response to [`GetBlockReplays`], containing one or more consecutive replay records.
#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
pub struct BlockReplays<T: WireReplayRecord> {
    pub records: Vec<T>,
}

impl<T: WireReplayRecord> BlockReplays<T> {
    pub fn new(records: Vec<StorageReplayRecord>) -> Self {
        let records = records.into_iter().map(T::from).collect();
        Self { records }
    }
}
