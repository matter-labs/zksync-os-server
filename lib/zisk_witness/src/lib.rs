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
//! The crate depends only on other lib crates, [`versioned_merkle_tree`] and
//! the guest library. It has no tie to the server binary, so both the batcher
//! seal path and the prover input generator call into it from `node/bin`.

mod batch;
mod bytes;
mod input_builder;

pub use batch::{ZiskChainConfig, assemble_batch};
pub use bytes::ZiskBlockBytes;
pub use input_builder::{
    ZiskBlockData, account_flat_key, build_block_witness, recover_code_matching,
    spec_id_from_execution_version,
};
