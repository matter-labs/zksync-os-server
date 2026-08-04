//! ZiSK second-proof-system configuration the batcher holds at seal time.
//!
//! The witness / `BatchInput` assembly lives in the `zisk_witness` lib crate
//! (behind its `assemble_batch` entrypoint), and the ZiSK proving lane (job
//! managers, metrics, commitment, shadow execution) lives in `zisk_prover_lane`.
//! This module keeps only the node-side configuration carriers the batcher
//! passes into those crates.

use std::sync::Arc;
use zisk_prover_lane::ZiskJobManager;
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
    /// ZiSK proving lane. The batcher opens a per-batch ZiSK job here at seal,
    /// so the ZiSK lane proves independently of the Airbender FRI lane; the
    /// Airbender SNARK submission is only the multi-proof rendezvous point. When
    /// the active queue is full, `add_job` parks the input in the lane's bounded
    /// backlog, so a full queue never blocks the seal and never drops the input.
    pub zisk_job_manager: Arc<ZiskJobManager>,
    /// Last batch already proved on L1 at startup. Batches at or below it are
    /// recreated during startup catch-up but need no ZiSK job (their proof is
    /// on L1). Mirrors the Airbender FRI lane, which skipped these; a startup
    /// snapshot, exactly like the FRI lane used.
    pub last_proved_batch: u64,
    /// Merkle tree handle for the batch-boundary tree views. Passed into
    /// `zisk_witness::assemble_batch`, which builds the views from it.
    pub merkle_tree: MerkleTree<RocksDBWrapper>,
    /// Shadow execution. `None` turns shadow execution off.
    pub shadow: Option<ShadowConfig>,
}
