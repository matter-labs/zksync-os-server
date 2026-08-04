//! Typed wrappers for the second proof-system (ZiSK) serialized payloads.
//!
//! The server moves two distinct ZiSK payloads through node-internal channels.
//! Each wrapper marks one payload so the two never mix by accident. Both stay
//! off the shared `ProverInput`. Both wrapper types live in lib crates and are
//! re-exported here so the node channel paths name them from one module.
//!
//! The batch-level [`ZiskBatchBytes`] is the cache payload of the ZiSK proving
//! lane, so it lives in `zisk_prover_lane`. The per-block [`ZiskBlockBytes`] is
//! the batch-assembly input, so it lives in `zisk_witness`.

/// Serialized batch-level ZiSK `BatchInput` (bincode). Defined in the ZiSK
/// proving-lane crate (its data-cache payload); re-exported so the node paths
/// stay stable.
pub use zisk_prover_lane::ZiskBatchBytes;

/// Serialized per-block ZiSK `ZiskBlockData` (bincode). Defined in the
/// `zisk_witness` crate (the batch-assembly input); re-exported so the node
/// paths stay stable.
pub use zisk_witness::ZiskBlockBytes;
