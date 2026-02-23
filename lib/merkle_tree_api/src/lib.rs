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
    /// Returns tree depth. Should return a constant value for a tree instance.
    fn tree_depth(&self) -> u8;

    /// Returns `Ok(None)` iff the version doesn't exist in the tree.
    fn prove(
        &self,
        version: u64,
        keys: &[B256],
    ) -> anyhow::Result<Option<(BatchTreeProof, TreeBatchOutput)>>;

    fn prove_for_api(
        &self,
        version: u64,
        keys: &[B256],
    ) -> anyhow::Result<Option<Vec<api::StorageSlotProof>>> {
        let Some((proof, batch_output)) = self.prove(version, keys)? else {
            return Ok(None);
        };
        let proofs = proof
            .to_api(self.tree_depth(), batch_output.leaf_count)
            .zip(keys)
            .map(|(proof, key)| api::StorageSlotProof { key: *key, proof });
        Ok(Some(proofs.collect()))
    }
}
