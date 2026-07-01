//! Off-chain Indexed Merkle Tree (IMT) engine for atomic-interop inclusion proofs.
//!
//! Faithful Rust port of the off-chain engine used to build `ImtInclusionProof`
//! (`IAtomicInterop.sol`). It reconstructs a chain's atomic-interop commitment tree from its
//! index-ordered leaf set (read via `L2InteropCommitmentTree.leafCount()` / `leafAt(i)`) and
//! reproduces the root and Merkle paths exactly as the on-chain `IndexedMerkleTree` /
//! `FullMerkle` libraries (`common/libraries/IndexedMerkleTree.sol` +
//! `common/libraries/FullMerkle.sol`, #2235) do, so a path produced here verifies under the
//! contract's `verifyInclusion`.
//!
//! This is a byte-for-byte port of the DYNAMIC-height `FullMerkle` tree (NOT the old
//! fixed-depth-32 model): the tree starts at height 0 and grows by one whenever a leaf is pushed
//! at index == `1 << height`. `root()` is the node at the current top level and `merkle_path(i)`
//! has length == the current height. The TypeScript reference implementation is
//! `l1-contracts/test/anvil-interop/src/helpers/imt-engine-lib.ts`.
//!
//! Hashing must match the contract bit-for-bit:
//! - leaf hash:  `keccak256(abi.encode(value, nextIndex, nextValue))` — three left-padded 32-byte words
//! - node hash:  `keccak256(left ++ right)` (`Merkle.sol`'s `efficientHash`)
//! - empty subtrees use lazily grown `zeros[level]`, with `zeros[0] = leafHash({0,0,0})` and
//!   `zeros[i+1] = efficientHash(zeros[i], zeros[i])`.

// The engine's public API is consumed by the atomic-interop proof RPC; allow until every helper is
// wired so an unused helper doesn't trip `-D warnings`.
#![allow(dead_code)]

use alloy::primitives::{B256, U256, keccak256};

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

/// The leaf hash of the `{0,0,0}` sentinel — `zeros[0]` in the FullMerkle sense.
fn zero_leaf_hash() -> B256 {
    indexed_leaf_hash(&ImtLeaf {
        value: U256::ZERO,
        next_index: U256::ZERO,
        next_value: U256::ZERO,
    })
}

/// Dynamic-height Indexed Merkle Tree, a byte-for-byte off-chain port of `FullMerkle` +
/// `IndexedMerkleTree` (#2235). It replays the EXACT on-chain build sequence (`setup` ->
/// `pushNewLeaf` per leaf, with `updateLeaf` rehashing the populated path) so that `root()` and
/// `merkle_path(i)` equal the on-chain `tree.root()` / `tree.merklePath(i)`.
///
/// The constructor takes the index-ordered leaf set (index 0 = the `{0,0,0}` sentinel head, exactly
/// what `setup` seeds). It does NOT re-derive the sorted linked list; the leaves passed in are the
/// live on-chain leaf preimages, so their `nextIndex`/`nextValue` are already spliced. Only the
/// FullMerkle node bookkeeping is replayed here.
///
/// `FullMerkle` storage mirror:
///   - `height`             : current tree height (0 for a single-leaf tree),
///   - `nodes[level][index]`: written node hashes (dynamic arrays, matching `_nodes`),
///   - `zeros[level]`       : zero-subtree hash at `level` (matching `_zeros`),
///   - `leaf_number`        : number of leaves pushed so far.
pub struct IndexedMerkleTree {
    /// Index-ordered leaf preimages (leaf 0 = head sentinel).
    leaves: Vec<ImtLeaf>,
    /// `_nodes[level][index]` — populated node hashes; higher indices are implicitly `zeros[level]`.
    nodes: Vec<Vec<B256>>,
    /// `_zeros[level]` — zero-subtree hash per level, grown lazily with the tree.
    zeros: Vec<B256>,
    /// `_height` — current top level.
    height: usize,
    /// `_leafNumber` — leaves pushed so far.
    leaf_number: u64,
}

impl IndexedMerkleTree {
    /// Reconstruct the tree from its index-ordered leaf set, replaying the on-chain build sequence.
    ///
    /// Panics if `leaves` is empty — a live commitment tree always has at least the `{0,0,0}`
    /// sentinel at index 0 (seeded by `IndexedMerkleTree.setup`), so an empty leaf set is a
    /// programming error in the caller, not a recoverable condition.
    pub fn new(leaves: Vec<ImtLeaf>) -> Self {
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

        // Mirror IndexedMerkleTree.setup: FullMerkle.setup(zeroLeafHash) seeds zeros[0] + nodes[0]=[zero],
        // then pushNewLeaf(zeroLeafHash) inserts the sentinel {0,0,0} at index 0.
        let zero_leaf = zero_leaf_hash();
        tree.setup(zero_leaf);
        tree.push_new_leaf(zero_leaf);

        // `setup`/`push_new_leaf` above seed index 0 from a pristine {0,0,0} sentinel. In a live tree
        // the head leaf has been repointed (its nextIndex/nextValue splice to the smallest inserted
        // value), so re-write index 0 with its actual on-chain preimage before pushing 1..n in order.
        let head_hash = indexed_leaf_hash(&tree.leaves[0]);
        tree.update_leaf(0, head_hash);
        for i in 1..tree.leaves.len() {
            let hash = indexed_leaf_hash(&tree.leaves[i]);
            tree.push_new_leaf(hash);
        }

        tree
    }

    // ── FullMerkle port ─────────────────────────────────────────────────────────────────────

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
            if index % 2 == 0 {
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
    pub fn root(&self) -> B256 {
        self.nodes[self.height][0]
    }

    /// `FullMerkle.merklePath`: dynamic-length path (length == current height) for the leaf at `index`.
    pub fn merkle_path(&self, start_index: u64) -> Vec<B256> {
        assert!(self.leaf_number != 0, "MerkleNothingToProve");
        let mut max_node_number = self.leaf_number - 1;
        assert!(
            start_index <= max_node_number,
            "MerkleWrongIndex({start_index}, {max_node_number})"
        );
        let mut index = start_index as usize;
        let mut proof = Vec::with_capacity(self.height);
        for i in 0..self.height {
            let sibling = if index % 2 == 0 {
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

    // ── Public accessors / lookup helpers (consumed by zks_impl.rs) ──────────────────────────

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

    /// Under the dynamic-height FullMerkle model a tree with only the `{0,0,0}` head leaf has
    /// height 0, so its root is simply that leaf's hash (`zeros[0]`), NOT a fixed top-level zero.
    #[test]
    fn seed_only_tree_root_is_leaf_hash() {
        let tree = IndexedMerkleTree::new(vec![leaf(0, 0, 0)]);
        assert_eq!(tree.root(), zero_leaf_hash());
        // Height 0 => an empty Merkle path.
        assert!(tree.merkle_path(0).is_empty());
    }

    /// Every leaf's produced Merkle path must recompute the tree root (path/verify round-trip),
    /// exactly as the on-chain `verifyInclusion` walks it. Under dynamic height the path length is
    /// the current tree height, not a fixed depth.
    #[test]
    fn merkle_paths_recompute_root() {
        // Head + two inserted leaves (linked-list order is irrelevant to the Merkle structure).
        let leaves = vec![leaf(0, 1, 5), leaf(5, 2, 9), leaf(9, 0, 0)];
        let tree = IndexedMerkleTree::new(leaves.clone());
        let root = tree.root();
        // 3 leaves => height 2 (leaf pushed at index 2 grows height from 1 to 2 only at index 4;
        // 3 leaves occupy indices 0..2, so height is 2).
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

    /// A tree that exactly fills a power-of-two leaf count (index of last leaf == (1<<h)-1) and one
    /// that has just triggered a height bump both self-verify — guards the height-growth branch.
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
