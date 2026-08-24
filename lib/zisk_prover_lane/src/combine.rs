//! Pure multi-proof composition for the second proof system (ZiSK).
//!
//! A MultiProof pairs one Airbender range SNARK with one aggregated ZiSK range
//! proof of the same bounds. This is the pure payload assembly: given the two
//! proofs and the proving version, it builds the L1 [`MultiProofSnarkProof`].
//!
//! The rendezvous orchestration that decides WHEN to compose — reading the
//! `SnarkJobManager` job map, parking/taking proofs, gating on
//! `require_multi_proof` — lives in `node/bin` and calls this function at the
//! moment both proofs are in hand.

use zksync_os_batch_types::batcher_model::{MultiProofShapeError, MultiProofSnarkProof};
use zksync_os_types::ProvingVersion;

/// Build the L1 MultiProof payload from the Airbender range SNARK and the
/// aggregated ZiSK range proof. The ZiSK public values are not carried in the
/// payload; the on-chain MultiProofVerifier reconstructs and binds them.
pub fn compose_multiproof(
    airbender_proof: Vec<u8>,
    zisk_proof: Vec<u8>,
    proving_version: ProvingVersion,
) -> Result<MultiProofSnarkProof, MultiProofShapeError> {
    MultiProofSnarkProof::new(airbender_proof, zisk_proof, proving_version as u32)
}
