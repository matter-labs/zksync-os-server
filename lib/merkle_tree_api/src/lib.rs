//! ZK OS Merkle tree API.

use std::fmt;

use alloy::primitives::B256;
pub use zksync_os_crypto::hasher::{Hasher, blake2::Blake2Hasher};

pub use crate::{
    hasher::HashTree,
    proofs::{BatchTreeProof, IntermediateHash, MerkleTreeView, TreeOperation},
    types::{Leaf, MAX_TREE_DEPTH, TreeBatchOutput, TreeEntry},
};

pub mod flat;
mod hasher;
mod proofs;
mod types;

/// Provider of Merkle tree proof data.
pub trait MerkleTreeProver: Send + Sync + fmt::Debug {
    /// Returns the tree depth. Should return a constant value for a tree instance.
    ///
    /// This is defined as an instance method to keep the trait dyn-compatible.
    fn tree_depth(&self) -> u8;

    /// Returns a batch proof of existence / absence for the requested `keys` in the tree at the specified
    /// `version`.
    ///
    /// Returns `Ok(None)` iff the version doesn't exist in the tree.
    ///
    /// # Errors
    ///
    /// Proxies I/O errors.
    fn prove(
        &self,
        version: u64,
        keys: &[B256],
    ) -> anyhow::Result<Option<(BatchTreeProof, TreeBatchOutput)>>;

    /// Specialized version of [`Self::prove()`] that receives an existing 0-based leaf index instead of the leaf key
    /// as an input.
    ///
    /// # Errors
    ///
    /// In addition to [`Self::prove()`] errors, this method will error if a leaf with `index`
    /// does not exist in the specified tree `version`.
    fn prove_index(
        &self,
        version: u64,
        index: u64,
    ) -> anyhow::Result<Option<(BatchTreeProof, TreeBatchOutput)>>;

    /// Returns flattened proofs of existence / absence for each of the requested `keys` in the tree at the specified
    /// `version`. The proofs are returned in the order of keys.
    ///
    /// Returns `Ok(None)` iff the version doesn't exist in the tree.
    ///
    /// This provided method should not be redefined in implementations.
    ///
    /// # Errors
    ///
    /// Proxies I/O errors.
    fn prove_flat(
        &self,
        version: u64,
        keys: &[B256],
    ) -> anyhow::Result<Option<(Vec<flat::StorageSlotProof>, TreeBatchOutput)>> {
        let Some((proof, batch_output)) = self.prove(version, keys)? else {
            return Ok(None);
        };
        let proofs = proof
            .to_flat(self.tree_depth(), batch_output.leaf_count)
            .zip(keys)
            .map(|(proof, key)| flat::StorageSlotProof { key: *key, proof });
        let proofs = proofs.collect();
        Ok(Some((proofs, batch_output)))
    }

    /// Same as [`Self::prove_index()`], but returns a flattened proof.
    fn prove_index_flat(
        &self,
        version: u64,
        index: u64,
    ) -> anyhow::Result<Option<flat::StorageSlotProofEntryWithKey>> {
        let Some((proof, batch_output)) = self.prove_index(version, index)? else {
            return Ok(None);
        };

        let leaf = *proof
            .sorted_leaves
            .values()
            .next()
            .expect("batch proof doesn't contain requested data");
        let proof = proof
            .to_flat(self.tree_depth(), batch_output.leaf_count)
            .next()
            .expect("batch proof doesn't contain requested data");
        let flat::InnerStorageSlotProof::Existing(entry) = proof else {
            unreachable!("requested index must exist in tree");
        };

        Ok(Some(flat::StorageSlotProofEntryWithKey {
            inner: entry,
            leaf_key: leaf.key,
        }))
    }
}
