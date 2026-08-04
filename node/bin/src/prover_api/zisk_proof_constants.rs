//! Shared constants for ZiSK proof sizes.
//!
//! These are invariants of the ZiSK Plonk verifier. The SNARK proof size lives
//! in `zksync_os_batch_types` so both the server and `l1_sender` share one
//! definition; the public-values size lives with the proving lane that
//! validates it. Both are re-exported here for the server-side call sites.

pub use zisk_prover_lane::ZISK_PUBLIC_VALUES_BYTES;
pub use zksync_os_batch_types::batcher_model::ZISK_SNARK_PROOF_BYTES;
