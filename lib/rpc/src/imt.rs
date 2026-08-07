//! Reconstructs atomic-interop Indexed Merkle Tree (IMT) proofs from historical leaf data.
//!
//! `L2InteropCommitmentTree` exposes its index-ordered leaf preimages, but not historical Merkle
//! paths. The proof RPC reads those leaves at the requested block and uses this module to replay the
//! contract's `IndexedMerkleTree` and `FullMerkle` layout.
//!
//! The tree has dynamic height. It begins at height 0 and grows when a leaf is pushed at
//! `index == 1 << height`, so a proof contains one sibling per current level rather than a fixed 32
//! siblings.
//!
//! Hashing must match the contract bit-for-bit:
//!
//! - Leaf hash: `keccak256(abi.encode(value, nextIndex, nextValue))`.
//! - Node hash: `keccak256(left ++ right)`.
//! - Empty subtrees use lazily grown `zeros[level]`, with `zeros[0] = leafHash({0,0,0})` and
//!   `zeros[i+1] = efficientHash(zeros[i], zeros[i])`.

use alloy::primitives::{B256, U256, keccak256};

/// Leaf preimage stored by the on-chain indexed tree.
///
/// Leaves occupy Merkle indices in insertion order. The `next_*` fields separately link them in
/// value order so the contract can prove that a value is present or bracketed by two leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImtLeaf {
    pub(crate) value: U256,
    pub(crate) next_index: U256,
    pub(crate) next_value: U256,
}

/// Hashes a leaf using the three-word Solidity ABI layout expected by the contract.
pub(crate) fn indexed_leaf_hash(leaf: &ImtLeaf) -> B256 {
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

/// The leaf hash of the `{0,0,0}` sentinel — `zeros[0]` in the FullMerkle sense.
fn zero_leaf_hash() -> B256 {
    indexed_leaf_hash(&ImtLeaf {
        value: U256::ZERO,
        next_index: U256::ZERO,
        next_value: U256::ZERO,
    })
}

/// Replays the contract's dynamic `FullMerkle` layout over stored IMT leaf preimages.
///
/// Input leaves must be ordered by Merkle index, with the sentinel head at index 0. Their
/// `next_index` and `next_value` already describe the value-sorted linked list; this type only
/// rebuilds the Merkle nodes needed for the root and paths.
pub(crate) struct IndexedMerkleTree {
    /// Index-ordered leaf preimages (leaf 0 = head sentinel).
    leaves: Vec<ImtLeaf>,
    /// Populated node hashes; higher indices are implicitly `zeros[level]`.
    nodes: Vec<Vec<B256>>,
    /// Zero-subtree hash per level, grown lazily with the tree.
    zeros: Vec<B256>,
    /// Current top level, matching `FullMerkle._height`.
    height: usize,
    /// Number of leaves replayed, matching `FullMerkle._leafNumber`.
    leaf_number: u64,
}

impl IndexedMerkleTree {
    /// Reconstruct the tree from its index-ordered leaf set, replaying the on-chain build sequence.
    ///
    /// Panics if `leaves` is empty — a live commitment tree always has at least the `{0,0,0}`
    /// sentinel at index 0 (seeded by `IndexedMerkleTree.setup`), so an empty leaf set is a
    /// programming error in the caller, not a recoverable condition.
    pub(crate) fn new(leaves: Vec<ImtLeaf>) -> Self {
        assert!(
            !leaves.is_empty(),
            "IndexedMerkleTree requires at least the sentinel leaf at index 0"
        );

        let mut tree = Self {
            leaves,
            nodes: Vec::new(),
            zeros: Vec::new(),
            height: 0,
            leaf_number: 0,
        };

        // The on-chain setup first establishes the zero hash, then inserts the pristine sentinel
        // as leaf 0.
        let zero_leaf = zero_leaf_hash();
        tree.setup(zero_leaf);
        tree.push_new_leaf(zero_leaf);

        // Later insertions repoint the sentinel toward the smallest value. Replace the pristine
        // leaf with the historical preimage before replaying the remaining insertion-order leaves.
        let head_hash = indexed_leaf_hash(&tree.leaves[0]);
        tree.update_leaf(0, head_hash);
        for i in 1..tree.leaves.len() {
            let hash = indexed_leaf_hash(&tree.leaves[i]);
            tree.push_new_leaf(hash);
        }

        tree
    }

    /// `FullMerkle.setup`: push the zero value into `zeros[0]` and seed `nodes[0] = [zero]`.
    fn setup(&mut self, zero: B256) {
        self.zeros.push(zero);
        self.nodes.push(vec![zero]);
    }

    /// `FullMerkle.pushNewLeaf`: append a leaf, growing the tree height when `index == 1 << height`.
    fn push_new_leaf(&mut self, leaf: B256) -> B256 {
        let index = self.leaf_number;
        self.leaf_number += 1;

        if index == 1u64 << self.height {
            let new_height = self.height + 1;
            self.height = new_height;
            let top_zero = self.zeros[new_height - 1];
            let new_zero = efficient_hash(top_zero, top_zero);
            self.zeros.push(new_zero);
            self.nodes.push(vec![new_zero]);
        }
        if index != 0 {
            let mut old_max_node_number = index - 1;
            let mut max_node_number = index;
            for i in 0..self.height {
                if old_max_node_number == max_node_number {
                    break;
                }
                let zero = self.zeros[i];
                self.nodes[i].push(zero);
                max_node_number /= 2;
                old_max_node_number /= 2;
            }
        }
        self.update_leaf(index, leaf)
    }

    /// `FullMerkle.updateLeaf`: set the leaf hash at `index` and rehash the populated path to the root.
    fn update_leaf(&mut self, start_index: u64, item_hash: B256) -> B256 {
        let mut max_node_number = self.leaf_number - 1;
        assert!(
            start_index <= max_node_number,
            "MerkleWrongIndex({start_index}, {max_node_number})"
        );
        let mut index = start_index as usize;
        self.nodes[0][index] = item_hash;
        let mut current_hash = item_hash;
        for i in 0..self.height {
            if index.is_multiple_of(2) {
                let right = if max_node_number == index as u64 {
                    self.zeros[i]
                } else {
                    self.nodes[i][index + 1]
                };
                current_hash = efficient_hash(current_hash, right);
            } else {
                current_hash = efficient_hash(self.nodes[i][index - 1], current_hash);
            }
            index /= 2;
            max_node_number /= 2;
            self.nodes[i + 1][index] = current_hash;
        }
        current_hash
    }

    /// `FullMerkle.root`: the node at the current top level.
    pub(crate) fn root(&self) -> B256 {
        self.nodes[self.height][0]
    }

    /// `FullMerkle.merklePath`: dynamic-length path (length == current height) for the leaf at `index`.
    pub(crate) fn merkle_path(&self, start_index: u64) -> Vec<B256> {
        assert!(self.leaf_number != 0, "MerkleNothingToProve");
        let mut max_node_number = self.leaf_number - 1;
        assert!(
            start_index <= max_node_number,
            "MerkleWrongIndex({start_index}, {max_node_number})"
        );
        let mut index = start_index as usize;
        let mut proof = Vec::with_capacity(self.height);
        for i in 0..self.height {
            let sibling = if index.is_multiple_of(2) {
                if max_node_number == index as u64 {
                    self.zeros[i]
                } else {
                    self.nodes[i][index + 1]
                }
            } else {
                self.nodes[i][index - 1]
            };
            proof.push(sibling);
            index /= 2;
            max_node_number /= 2;
        }
        proof
    }

    pub(crate) fn leaves(&self) -> &[ImtLeaf] {
        &self.leaves
    }

    /// Index of the leaf holding `value`, or `None` if absent.
    pub(crate) fn find_value_index(&self, value: U256) -> Option<u64> {
        self.leaves
            .iter()
            .position(|l| l.value == value)
            .map(|i| i as u64)
    }

    /// Index of the low-nullifier leaf for `value`: `L.value < value` and
    /// (`L.nextValue == 0` or `value < L.nextValue`).
    pub(crate) fn find_low_nullifier_index(&self, value: U256) -> Option<u64> {
        self.leaves
            .iter()
            .position(|l| l.value < value && (l.next_value.is_zero() || value < l.next_value))
            .map(|i| i as u64)
    }
}

/// Walks a leaf-to-root path in the same order as the on-chain verifier.
///
/// The proof RPC uses this as a defensive check before returning a reconstructed path.
pub(crate) fn calculate_root(path: &[B256], index: u64, leaf_hash: B256) -> B256 {
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

    // A sentinel-only tree has height 0, so its root is the sentinel leaf hash and its path is
    // empty.
    #[test]
    fn seed_only_tree_root_is_leaf_hash() {
        let tree = IndexedMerkleTree::new(vec![leaf(0, 0, 0)]);
        assert_eq!(tree.root(), zero_leaf_hash());
        assert!(tree.merkle_path(0).is_empty());
    }

    // Every generated path should round-trip through the same leaf-to-root walk used on-chain.
    #[test]
    fn merkle_paths_recompute_root() {
        // Head + two inserted leaves (linked-list order is irrelevant to the Merkle structure).
        let leaves = vec![leaf(0, 1, 5), leaf(5, 2, 9), leaf(9, 0, 0)];
        let tree = IndexedMerkleTree::new(leaves.clone());
        let root = tree.root();
        // Pushing leaf index 2 reaches `1 << 1` and grows the tree to height 2.
        for (i, l) in leaves.iter().enumerate() {
            let path = tree.merkle_path(i as u64);
            assert_eq!(path.len(), tree.height);
            assert_eq!(
                calculate_root(&path, i as u64, indexed_leaf_hash(l)),
                root,
                "path for leaf {i} must recompute the root"
            );
        }
    }

    // Crossing a power-of-two leaf index grows the tree and must preserve every existing path.
    #[test]
    fn height_growth_paths_recompute_root() {
        // 5 leaves: pushing index 4 (== 1<<2) grows height from 2 to 3.
        let leaves = vec![
            leaf(0, 1, 3),
            leaf(3, 2, 7),
            leaf(7, 3, 11),
            leaf(11, 4, 20),
            leaf(20, 0, 0),
        ];
        let tree = IndexedMerkleTree::new(leaves.clone());
        assert_eq!(tree.height, 3);
        let root = tree.root();
        for (i, l) in leaves.iter().enumerate() {
            let path = tree.merkle_path(i as u64);
            assert_eq!(path.len(), 3);
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
