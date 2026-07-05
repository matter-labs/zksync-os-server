//! The versioned wire formats of replay records.
//!
//! A replay record is the node's self-contained unit for reproducing a block (full
//! signed transactions, block context, execution-outcome commitment). These formats
//! are how records travel and persist beyond one process: external-node sync streams
//! them, and consensus hashes them into block identities.
//!
//! Every released version is immutable: `vN.rs` files are never edited, only copied
//! into a `vN+1`. The golden tests in this crate enforce that byte-for-byte.

pub mod v0;
pub mod v1;
pub mod v2;
pub mod v3;

mod impls;

use alloy::consensus::crypto::RecoveryError;
use alloy::primitives::BlockNumber;
use alloy_rlp::{Decodable, Encodable};
use std::fmt::Debug;
use zksync_os_storage_api::ReplayRecord as StorageReplayRecord;

/// Represents any replay record wire format. It's expected to be convertable from/to
/// the replay record used by sequencer and storage layers.
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
