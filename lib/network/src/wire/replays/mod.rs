//! Replay request and response payloads for the `zks` protocol.
//!
//! The protocol version pins the replay record format used inside [`BlockReplays`]: production
//! `zks/5` uses [`v3`], while negotiation tests can use the deliberately lossy [`v0`].

pub mod v0;
pub mod v3;

mod impls;

use alloy::consensus::crypto::RecoveryError;
use alloy::primitives::{BlockNumber, Bytes};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use std::fmt::Debug;
use zksync_os_storage_api::ReplayRecord as StorageReplayRecord;

/// Opens a replay stream beginning at [`Self::starting_block`].
///
/// The peer keeps returning [`BlockReplays`] responses until the connection or its replay storage
/// closes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
#[rlp(trailing)]
pub struct GetBlockReplays {
    /// The block number that the peer should start returning replay blocks from.
    pub starting_block: u64,
    /// Records for which DB keys should be overridden. Used only for debugging.
    pub record_overrides: Vec<RecordOverride>,
    /// Requested maximum number of consecutive records in each response.
    ///
    /// `None` defaults to one record. The main node clamps a supplied value to its supported range.
    pub max_blocks_per_message: Option<u64>,
}

/// Asks the main node to read one replay record from an explicit database key.
///
/// This lets an external node debug a reverted block whose record is no longer canonical.
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

/// Conversion boundary between storage replay records and immutable wire formats.
///
/// Each `zks` protocol version pins one implementation, while the sequencer and storage layers
/// continue to use [`StorageReplayRecord`].
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
    /// Returns the record's block number without converting the full payload.
    fn block_number(&self) -> BlockNumber;
}
