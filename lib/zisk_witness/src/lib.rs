//! ZiSK witness construction and per-batch `BatchInput` assembly.
//!
//! This crate holds the server-lane logic that turns native block and batch
//! data into the second proof-system (ZiSK) guest input:
//!
//! - [`build_block_witness`] converts one block's server-side data into a
//!   [`ZiskBlockData`] with the merkle proofs the guest authenticates against.
//! - [`assemble_batch`] gathers a whole batch's batch-level witness inputs (tree
//!   views, after-state preimages, referenced bytecodes) and folds the per-block
//!   data into a single batch-level guest `BatchInput`, serialized with the
//!   guest's wire config. It is the single entrypoint the batcher seal path uses.
//!
//! The crate depends only on other lib crates, [`zksync_os_native_pig`] and
//! the guest library. It has no tie to the server binary: the batcher's seal
//! path is its one caller, which is also where the Airbender prover input is
//! built.

mod batch;
mod input_builder;

// The crate's entrypoints: the batcher's seal path builds a batch witness from
// one `ZiskSealBlock` per block, and both this crate and the proving lane
// commit to the same `ZiskChainConfig`. Everything below them is an
// implementation phase and stays crate-private, so those phases can be
// reshaped without a compatibility obligation.
pub use batch::{BatchWitnessContext, ZiskChainConfig, ZiskSealBlock, build_batch_witness};
// The shadow executor rebuilds the guest's account-properties key to compare
// its own state reads against the witness.
pub use input_builder::account_flat_key;
