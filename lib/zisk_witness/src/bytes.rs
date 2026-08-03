//! Typed wrapper for the per-block second proof-system (ZiSK) payload.

/// Serialized per-block ZiSK `ZiskBlockData` (bincode).
///
/// The prover input generator produces one value per block. The batcher
/// collects the values in block order and hands the slice to
/// [`crate::assemble_batch`], which folds them into the batch input.
#[derive(Clone, Debug)]
pub struct ZiskBlockBytes(pub Vec<u8>);

impl ZiskBlockBytes {
    /// Return the wrapped bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}
