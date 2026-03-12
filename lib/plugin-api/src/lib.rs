//! Stable trait definitions for multivm execution and proving plugins.
//!
//! Each execution version of zksync-os implements [`ExecutionPlugin`] to provide
//! block execution and transaction simulation capabilities. Each proving version
//! implements [`ProvingPlugin`] for batch proof input generation. These crates
//! deliberately have no dependency on any `forward_system` version — they only
//! depend on the version-stable `zksync_os_interface`.

use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::AnyTracer;
use zksync_os_interface::traits::{
    EncodedTx, PreimageSource, ReadStorage, TxResultCallback, TxSource,
};
use zksync_os_interface::types::{BlockContext, BlockOutput, TxOutput};

/// Trait for block execution and transaction simulation, implemented by each
/// version-specific plugin crate.
///
/// The trait uses generic parameters (not trait objects) to match the existing
/// `zksync_os_interface` design and avoid boxing overhead in the hot path.
pub trait ExecutionPlugin {
    fn run_block<S, P, T, C, Tr>(
        &self,
        block_context: BlockContext,
        storage: S,
        preimage_source: P,
        tx_source: T,
        tx_result_callback: C,
        tracer: &mut Tr,
    ) -> Result<BlockOutput, anyhow::Error>
    where
        S: ReadStorage,
        P: PreimageSource,
        T: TxSource,
        C: TxResultCallback,
        Tr: AnyTracer;

    fn simulate_tx<S, P, Tr>(
        &self,
        transaction: EncodedTx,
        block_context: BlockContext,
        storage: S,
        preimage_source: P,
        tracer: &mut Tr,
    ) -> Result<Result<TxOutput, InvalidTransaction>, anyhow::Error>
    where
        S: ReadStorage,
        P: PreimageSource,
        Tr: AnyTracer;
}

/// Trait for batch-level proof input generation.
///
/// Block-level proof input generation is version-specific and uses concrete types
/// (e.g. `MerkleTreeVersion`) that cannot be abstracted without pulling in
/// `forward_system` types. It is therefore exposed as a standalone function on each
/// proving plugin crate rather than through this trait.
pub trait ProvingPlugin {
    fn generate_batch_proof_input(
        &self,
        block_proof_inputs: Vec<&[u32]>,
        da_commitment_scheme: u8,
        pubdata: Vec<&[u8]>,
    ) -> Result<Vec<u32>, anyhow::Error>;
}
