use serde::{Deserialize, Serialize};

use crate::{InteropRootsLogIndex, L1TxSerialId};

/// Starting positions for the L1-backed inputs consumed while building a block.
///
/// Serde field names match the legacy flat fields on `ReplayRecord` so
/// `#[serde(flatten)]` remains backwards-compatible.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlockStartCursors {
    /// Next expected L1 priority transaction serial id (0-based).
    #[serde(rename = "starting_l1_priority_id")]
    pub l1_priority_id: L1TxSerialId,
    /// Position of the next interop root event to consume.
    #[serde(rename = "starting_interop_event_index")]
    pub interop_event_index: InteropRootsLogIndex,
    /// Next migration event number to consume.
    #[serde(rename = "starting_migration_number")]
    pub migration_number: u64,
    /// Next interop fee update number to consume.
    #[serde(rename = "starting_interop_fee_number")]
    pub interop_fee_number: u64,
}
