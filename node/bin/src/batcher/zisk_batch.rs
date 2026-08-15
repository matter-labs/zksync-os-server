//! ZiSK second-proof-system configuration the batcher holds at seal time.
//!
//! The witness assembly lives in the `zisk_witness` lib crate (behind its
//! `build_batch_witness` entrypoint) and the proving lane in
//! `zisk_prover_lane`. This module keeps only the configuration the batcher
//! needs at seal — no lane handles: the seal's products ride the batch
//! envelope, and the per-batch proving stage opens the job.

use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper};

// `ZiskChainConfig` lives in the `zisk_witness` lib crate. Re-export it so the
// existing `crate::batcher::zisk_batch::ZiskChainConfig` paths stay valid.
pub use zisk_witness::ZiskChainConfig;

/// Shadow-execution settings for the second proof system.
///
/// The batcher holds this only when shadow execution is on. It re-executes
/// every sealed batch's ZiSK input in-process and compares the batch public
/// input.
#[derive(Clone, Copy, Debug)]
pub struct ShadowConfig {
    /// Fail batch sealing on a shadow-execution divergence.
    pub halt_on_mismatch: bool,
}

/// All settings the batcher needs to build the second proof-system input.
///
/// The batcher holds `Some` only when the second proof system is on. `None`
/// disables the whole ZiSK seal path, so the batcher behaves like upstream.
#[derive(Clone)]
pub struct SecondProofSystemConfig {
    /// Chain-config parameters committed into the ZiSK batch public input.
    pub chain_config: ZiskChainConfig,
    /// Merkle tree handle for the batch-boundary tree views. The witness
    /// crate builds the views from it while assembling the batch input.
    pub merkle_tree: MerkleTree<RocksDBWrapper>,
    /// Shadow execution. `None` turns shadow execution off.
    pub shadow: Option<ShadowConfig>,
    /// Last batch already proved on L1 at startup. Startup catch-up re-seals
    /// these, but their proofs are on chain: they need no witness, so a build
    /// failure on one must never stop the seal — otherwise a deterministic
    /// failure on an already-settled batch is an unrecoverable crash loop.
    pub last_proved_batch: u64,
    /// The second proof system gates settlement (`multi_proof_verifier`). A
    /// batch whose ZiSK input is missing could never settle, so seal-time
    /// failures stop the seal instead of degrading the batch's ZiSK data — the
    /// stall is recoverable, a stranded committed batch is not.
    pub required: bool,
}
