//! ZK OS Merkle tree API.

use alloy::primitives::B256;
pub use zksync_os_crypto::hasher::{Hasher, blake2::Blake2Hasher};

pub use crate::{
    hasher::HashTree,
    proofs::{BatchTreeProof, IntermediateHash, TreeOperation},
    types::{Leaf, MAX_TREE_DEPTH, TreeBatchOutput, TreeEntry},
};

pub mod api;
mod hasher;
mod proofs;
mod types;

/// Provider of Merkle tree proof data.
pub trait MerkleTreeProver {
    /// Returns `Ok(None)` iff the version doesn't exist in the tree.
    fn prove(&self, version: u64, keys: &[B256]) -> anyhow::Result<Option<BatchTreeProof>>;
}
