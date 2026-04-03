//! Implements the `GetBlockReplays` and `BlockReplays` message types.
//!
//! `BlockReplays` is versioned over all the possible replay record wire formats supported by this
//! node.

pub mod v0;
pub mod v1;
pub mod v2;
pub mod v3;

mod impls;

use alloy::consensus::crypto::RecoveryError;
use alloy::primitives::{BlockNumber, Bytes};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use std::fmt::Debug;
use zksync_os_storage_api::ReplayRecord as StorageReplayRecord;

/// A request for a peer to return block replays starting at the requested block number.
/// The peer MUST start streaming indefinite number of [`BlockReplays`] responses.
#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
pub struct GetBlockReplays {
    /// The block number that the peer should start returning replay blocks from.
    pub starting_block: u64,
    /// Records for which DB keys should be overridden. Used only for debugging.
    pub record_overrides: Vec<RecordOverride>,
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

/// Represents any replay record wire format. It's expected to be convertable from/to replay record
/// used by sequencer and storage layers.
pub trait WireReplayRecord:
    From<StorageReplayRecord>
    + TryInto<StorageReplayRecord, Error = RecoveryError>
    + Encodable
    + Decodable
    + Debug
    + Send
    + Sync
    + Unpin
{
    /// Get record's block number.
    fn block_number(&self) -> BlockNumber;
}
