//! Off-chain Indexed Merkle Tree (IMT) engine for atomic-interop inclusion proofs.
//!
//! Faithful Rust port of the off-chain engine used to build `ImtInclusionProof`
//! (`IAtomicInterop.sol`). It reconstructs a chain's atomic-interop commitment tree from its
//! index-ordered leaf set (read via `L2InteropCommitmentTree.leafCount()` / `leafAt(i)`) and
//! reproduces the root and Merkle paths exactly as the on-chain `IndexedMerkleTreeLib`
//! (`common/libraries/IndexedMerkleTree.sol`) does, so a path produced here verifies under the
//! contract's `verifyInclusion`.
//!
//! Hashing must match the contract bit-for-bit:
//! - leaf hash:  `keccak256(abi.encode(value, nextIndex, nextValue))` — three left-padded 32-byte words
//! - node hash:  `keccak256(left ++ right)` (`Merkle.sol`'s `efficientHash`)
//! - empty subtrees use precomputed `zeros[level]`, with `zeros[0] = leafHash({0,0,0})`.

// The engine's public API is consumed by the (forthcoming) atomic-interop proof RPC; allow until
// that handler lands so an unwired module doesn't trip `-D warnings`.
#![allow(dead_code)]

use std::collections::HashMap;

use alloy::primitives::{keccak256, B256, U256};

/// Fixed tree depth (matches `IndexedMerkleTreeLib`).
pub const IMT_DEPTH: usize = 32;

/// Indexed-tree leaf, in the on-chain field order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImtLeaf {
    pub value: U256,
    pub next_index: U256,
    pub next_value: U256,
}

/// `keccak256(abi.encode(value, nextIndex, nextValue))`.
pub fn indexed_leaf_hash(leaf: &ImtLeaf) -> B256 {
    let mut buf = [0u8; 96];
    buf[0..32].copy_from_slice(&leaf.value.to_be_bytes::<32>());
    buf[32..64].copy_from_slice(&leaf.next_index.to_be_bytes::<32>());
    buf[64..96].copy_from_slice(&leaf.next_value.to_be_bytes::<32>());
    keccak256(buf)
}

/// `keccak256(left ++ right)` — matches `Merkle.sol`'s `efficientHash`.
fn efficient_hash(left: B256, right: B256) -> B256 {
    let mut buf = [0u8; 64];
    buf[0..32].copy_from_slice(left.as_slice());
    buf[32..64].copy_from_slice(right.as_slice());
    keccak256(buf)
}

/// `zeros[0] = leafHash({0,0,0})`; `zeros[i+1] = efficientHash(zeros[i], zeros[i])`. Length `IMT_DEPTH + 1`.
fn compute_zeros() -> [B256; IMT_DEPTH + 1] {
    let mut zeros = [B256::ZERO; IMT_DEPTH + 1];
    zeros[0] = indexed_leaf_hash(&ImtLeaf {
        value: U256::ZERO,
        next_index: U256::ZERO,
        next_value: U256::ZERO,
    });
    for i in 0..IMT_DEPTH {
        zeros[i + 1] = efficient_hash(zeros[i], zeros[i]);
    }
    zeros
}

/// Sparse fixed-depth Indexed Merkle Tree reconstructed from the index-ordered leaf set
/// (`leaves[0]` is the `{0,0,0}` head). Mirrors the on-chain `IMT` storage: `nodes[level][index]`
/// holds written nodes; unwritten siblings default to `zeros[level]`.
pub struct IndexedMerkleTree {
    leaves: Vec<ImtLeaf>,
    zeros: [B256; IMT_DEPTH + 1],
    /// `nodes[level]: index -> hash` — only populated path nodes are materialized.
    nodes: Vec<HashMap<u64, B256>>,
}

impl IndexedMerkleTree {
    pub fn new(leaves: Vec<ImtLeaf>) -> Self {
        let zeros = compute_zeros();
        let mut nodes: Vec<HashMap<u64, B256>> =
            (0..=IMT_DEPTH).map(|_| HashMap::new()).collect();

        // Level 0: write each leaf hash at its index.
        for (i, leaf) in leaves.iter().enumerate() {
            nodes[0].insert(i as u64, indexed_leaf_hash(leaf));
        }

        // Build every higher level from the parents of populated children (and their zero-filled
        // siblings), so a node is materialized iff at least one descendant leaf is populated —
        // matching the on-chain `_updatePath` write set, hence identical roots / paths.
        for level in 0..IMT_DEPTH {
            let parents: std::collections::BTreeSet<u64> =
                nodes[level].keys().map(|child| child >> 1).collect();
            for parent in parents {
                let left_index = parent * 2;
                let left = Self::node_at(&nodes, &zeros, level, left_index);
                let right = Self::node_at(&nodes, &zeros, level, left_index + 1);
                nodes[level + 1].insert(parent, efficient_hash(left, right));
            }
        }

        Self {
            leaves,
            zeros,
            nodes,
        }
    }

    /// Read a node, falling back to the level's zero hash when unwritten.
    fn node_at(
        nodes: &[HashMap<u64, B256>],
        zeros: &[B256; IMT_DEPTH + 1],
        level: usize,
        index: u64,
    ) -> B256 {
        nodes[level]
            .get(&index)
            .copied()
            .unwrap_or(zeros[level])
    }

    /// The current IMT root (level `IMT_DEPTH`, index 0).
    pub fn root(&self) -> B256 {
        Self::node_at(&self.nodes, &self.zeros, IMT_DEPTH, 0)
    }

    /// Fixed-depth Merkle path (32 siblings, leaf level up) for the leaf at `index`.
    pub fn merkle_path(&self, index: u64) -> Vec<B256> {
        let mut path = Vec::with_capacity(IMT_DEPTH);
        let mut idx = index;
        for level in 0..IMT_DEPTH {
            let sibling = idx ^ 1;
            path.push(Self::node_at(&self.nodes, &self.zeros, level, sibling));
            idx >>= 1;
        }
        path
    }

    pub fn leaves(&self) -> &[ImtLeaf] {
        &self.leaves
    }

    /// Index of the leaf holding `value`, or `None` if absent.
    pub fn find_value_index(&self, value: U256) -> Option<u64> {
        self.leaves
            .iter()
            .position(|l| l.value == value)
            .map(|i| i as u64)
    }

    /// Index of the low-nullifier leaf for `value`: `L.value < value` and
    /// (`L.nextValue == 0` or `value < L.nextValue`).
    pub fn find_low_nullifier_index(&self, value: U256) -> Option<u64> {
        self.leaves
            .iter()
            .position(|l| l.value < value && (l.next_value.is_zero() || value < l.next_value))
            .map(|i| i as u64)
    }
}

/// Recompute a root from a leaf hash + its Merkle path — mirrors `Merkle.calculateRootMemory`.
/// Useful for asserting a produced path verifies before returning it.
pub fn calculate_root(path: &[B256], index: u64, leaf_hash: B256) -> B256 {
    let mut current = leaf_hash;
    let mut idx = index;
    for sibling in path {
        current = if idx & 1 == 0 {
            efficient_hash(current, *sibling)
        } else {
            efficient_hash(*sibling, current)
        };
        idx >>= 1;
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(value: u64, next_index: u64, next_value: u64) -> ImtLeaf {
        ImtLeaf {
            value: U256::from(value),
            next_index: U256::from(next_index),
            next_value: U256::from(next_value),
        }
    }

    /// A tree with only the `{0,0,0}` head leaf must have root == zeros[IMT_DEPTH], since leaf 0's
    /// hash equals zeros[0] and every sibling up the path is the zero subtree.
    #[test]
    fn seed_only_tree_root_is_top_zero() {
        let zeros = compute_zeros();
        let tree = IndexedMerkleTree::new(vec![leaf(0, 0, 0)]);
        assert_eq!(tree.root(), zeros[IMT_DEPTH]);
    }

    /// Every leaf's produced Merkle path must recompute the tree root (path/verify round-trip),
    /// exactly as the on-chain `verifyInclusion` walks it.
    #[test]
    fn merkle_paths_recompute_root() {
        // Head + two inserted leaves (linked-list order is irrelevant to the Merkle structure).
        let leaves = vec![leaf(0, 1, 5), leaf(5, 2, 9), leaf(9, 0, 0)];
        let tree = IndexedMerkleTree::new(leaves.clone());
        let root = tree.root();
        for (i, l) in leaves.iter().enumerate() {
            let path = tree.merkle_path(i as u64);
            assert_eq!(path.len(), IMT_DEPTH);
            assert_eq!(
                calculate_root(&path, i as u64, indexed_leaf_hash(l)),
                root,
                "path for leaf {i} must recompute the root"
            );
        }
    }

    #[test]
    fn find_helpers() {
        let tree = IndexedMerkleTree::new(vec![leaf(0, 1, 5), leaf(5, 2, 9), leaf(9, 0, 0)]);
        assert_eq!(tree.find_value_index(U256::from(5)), Some(1));
        assert_eq!(tree.find_value_index(U256::from(7)), None);
        // low-nullifier for 7: leaf with value 5 (5 < 7 < 9) at index 1.
        assert_eq!(tree.find_low_nullifier_index(U256::from(7)), Some(1));
        // low-nullifier for 100: leaf with value 9 (nextValue == 0) at index 2.
        assert_eq!(tree.find_low_nullifier_index(U256::from(100)), Some(2));
    }
}
