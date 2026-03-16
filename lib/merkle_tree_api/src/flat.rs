//! Merkle tree-related types suitable for use in RPC.

use std::collections::BTreeMap;

use alloy::primitives::B256;
use serde::{Deserialize, Serialize};

use crate::{BatchTreeProof, Blake2Hasher, HashTree, Leaf, TreeOperation};

/// Information about a Merkle tree leaf sufficient (together with the storage slot key) to recover
/// the tree root hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSlotProofEntry {
    pub index: u64,
    pub value: B256,
    pub next_index: u64,
    /// Merkle path to the slot in the leaf-to-root order. May contain fewer entries than `tree_depth - 1`;
    /// in this case, should be padded at the end by hashes of empty subtrees at the corresponding depth.
    pub siblings: Vec<B256>,
}

impl StorageSlotProofEntry {
    fn hash(&self, tree_depth: u8, leaf_key: B256) -> anyhow::Result<B256> {
        anyhow::ensure!(self.siblings.len() < usize::from(tree_depth));

        let leaf = Leaf {
            key: leaf_key,
            value: self.value,
            next_index: self.next_index,
        };
        let mut hash = Blake2Hasher.hash_leaf(&leaf);
        let mut index = self.index;
        for depth in 0..tree_depth {
            let sibling_hash = self
                .siblings
                .get(usize::from(depth))
                .copied()
                .unwrap_or_else(|| Blake2Hasher.empty_subtree_hash(depth));
            hash = if index.is_multiple_of(2) {
                Blake2Hasher.hash_branch(&hash, &sibling_hash)
            } else {
                Blake2Hasher.hash_branch(&sibling_hash, &hash)
            };
            index /= 2;
        }
        Ok(hash)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NeighborStorageSlotProofEntry {
    #[serde(flatten)]
    pub inner: StorageSlotProofEntry,
    pub leaf_key: B256,
}

impl NeighborStorageSlotProofEntry {
    fn hash(&self, tree_depth: u8) -> anyhow::Result<B256> {
        self.inner.hash(tree_depth, self.leaf_key)
    }
}

/// Proof for a single Merkle tree storage slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum InnerStorageSlotProof {
    /// The slot is present in the tree.
    Existing(StorageSlotProofEntry),
    /// The slot is missing from the tree.
    NonExisting {
        /// Proof for the left neighbor of the slot present in the tree.
        left_neighbor: NeighborStorageSlotProofEntry,
        /// Proof for the right neighbor of the slot present in the tree.
        right_neighbor: NeighborStorageSlotProofEntry,
    },
}

impl InnerStorageSlotProof {
    /// Verifies this proof. `key` refers to the slot in the tree (i.e., the *flat* key in terms of ZKsync OS).
    pub fn verify(&self, tree_depth: u8, key: B256) -> anyhow::Result<B256> {
        match self {
            Self::Existing(entry) => entry.hash(tree_depth, key),
            Self::NonExisting {
                left_neighbor,
                right_neighbor,
            } => {
                anyhow::ensure!(left_neighbor.leaf_key < key);
                anyhow::ensure!(key < right_neighbor.leaf_key);
                anyhow::ensure!(left_neighbor.inner.next_index == right_neighbor.inner.index);

                let root_hash_left = left_neighbor.hash(tree_depth)?;
                let root_hash_right = right_neighbor.hash(tree_depth)?;
                anyhow::ensure!(root_hash_left == root_hash_right);
                Ok(root_hash_left)
            }
        }
    }
}

/// Storage proof for a single Merkle tree slot + the slot key (to allow for standalone verification).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSlotProof<K = B256> {
    /// Key of the slot in the tree. For proofs produced by the `merkle_tree` crate, this is the *flat* key
    /// in terms of ZKsync OS, which allows for standalone proof verification.
    pub key: K,
    /// Proof contents.
    pub proof: InnerStorageSlotProof,
}

impl StorageSlotProof {
    /// Verifies the internal consistency of this proof and returns the recovered tree root hash.
    pub fn verify(&self, tree_depth: u8) -> anyhow::Result<B256> {
        self.proof.verify(tree_depth, self.key)
    }
}

impl<K> StorageSlotProof<K> {
    /// Returns the storage value for the slot.
    pub fn value(&self) -> Option<B256> {
        match &self.proof {
            InnerStorageSlotProof::Existing(entry) => Some(entry.value),
            InnerStorageSlotProof::NonExisting { .. } => None,
        }
    }
}

impl BatchTreeProof {
    /// Converts this proof to the flat format by filling Merkle paths that are implicitly present
    /// in the proof.
    pub(crate) fn to_flat(
        &self,
        tree_depth: u8,
        leaf_count: u64,
    ) -> impl Iterator<Item = InnerStorageSlotProof> {
        assert!(self.operations.is_empty());

        // Get all sibling hashes – essentially, an extension of `self.hashes` that allow to construct
        // Merkle paths for all returned `InnerStorageSlotProof`s. The hashes will be ordered
        // in the same way they will be queried below; the order correctness is asserted
        // (see the `get_sibling_hash` closure).
        let mut sibling_hashes = vec![];
        Self::zip_leaves(
            &Blake2Hasher,
            tree_depth,
            leaf_count,
            self.sorted_leaves.iter().map(|(idx, leaf)| (*idx, leaf)),
            self.hashes.iter(),
            Some(&mut sibling_hashes),
        )
        .expect("invalid batch tree proof");

        let proof_entries = self.sorted_leaves.iter().map(|(&index, leaf)| {
            let proof_entry = StorageSlotProofEntry {
                index,
                value: leaf.value,
                next_index: leaf.next_index,
                siblings: vec![],
            };
            (index, proof_entry)
        });
        let mut proof_entries: BTreeMap<_, _> = proof_entries.collect();
        let mut indexes_on_level: Vec<_> = proof_entries
            .iter_mut()
            .map(|(idx, entry)| (*idx, entry))
            .collect();

        let mut sibling_idx = 0;
        let mut get_sibling_hash = move |depth: u8, idx: u64| -> B256 {
            let current = sibling_hashes[sibling_idx];
            if current.location == (depth, idx) {
                // We may query the same hash multiple times, e.g. if multiple proven slots have the same index
                // on upper levels.
                return current.value;
            }

            // If we've moved past the current sibling hash, the next one is always the one we need due to how `sibling_hashes`
            // are filled in `zip_leaves()`.
            sibling_idx += 1;
            let current = sibling_hashes[sibling_idx];
            assert_eq!(
                current.location,
                (depth, idx),
                "sibling hashes extracted incorrectly"
            );
            current.value
        };

        let mut last_idx_on_level = leaf_count - 1;
        for depth in 0..tree_depth {
            for (idx, entry) in &mut indexes_on_level {
                if *idx % 2 == 1 {
                    let sibling_hash = get_sibling_hash(depth, *idx - 1);
                    entry.siblings.push(sibling_hash);
                } else {
                    let sibling_hash = if *idx == last_idx_on_level {
                        Blake2Hasher.empty_subtree_hash(depth)
                    } else {
                        get_sibling_hash(depth, *idx + 1)
                    };
                    entry.siblings.push(sibling_hash);
                }
                *idx /= 2;
            }
            last_idx_on_level /= 2;
            if last_idx_on_level == 0 {
                // All further added hashes would correspond to empty subtrees; thus, we've finished building
                // sibling hashes.
                break;
            }
        }

        self.read_operations.iter().copied().map(move |op| {
            match op {
                TreeOperation::Hit { index } => {
                    // We cannot remove entries from `proof_entries` because the same entry can be used
                    // in multiple slot proofs, e.g. as an existing and neighboring paths.
                    let entry = proof_entries[&index].clone();
                    InnerStorageSlotProof::Existing(entry)
                }
                TreeOperation::Miss { prev_index } => {
                    let prev_entry = proof_entries[&prev_index].clone();
                    let prev_key = self.sorted_leaves[&prev_index].key;
                    let next_entry = proof_entries[&prev_entry.next_index].clone();
                    let next_key = self.sorted_leaves[&prev_entry.next_index].key;
                    InnerStorageSlotProof::NonExisting {
                        left_neighbor: NeighborStorageSlotProofEntry {
                            inner: prev_entry,
                            leaf_key: prev_key,
                        },
                        right_neighbor: NeighborStorageSlotProofEntry {
                            inner: next_entry,
                            leaf_key: next_key,
                        },
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use rand::{Rng, SeedableRng, rngs::StdRng};

    use super::*;

    fn random_entry(rng: &mut impl Rng) -> StorageSlotProofEntry {
        StorageSlotProofEntry {
            index: rng.random(),
            value: B256::random_with(rng),
            next_index: rng.random(),
            siblings: (0..rng.random_range(5..=15))
                .map(|_| B256::random_with(rng))
                .collect(),
        }
    }

    #[test]
    fn existing_proof_serialization_snapshot() {
        const RNG_SEED: u64 = 42;

        let mut rng = StdRng::seed_from_u64(RNG_SEED);
        let proof = StorageSlotProof {
            key: B256::random_with(&mut rng),
            proof: InnerStorageSlotProof::Existing(random_entry(&mut rng)),
        };

        insta::assert_yaml_snapshot!("existing_proof", proof);
    }

    #[test]
    fn missing_proof_serialization_snapshot() {
        const RNG_SEED: u64 = 123;

        let mut rng = StdRng::seed_from_u64(RNG_SEED);
        let proof = StorageSlotProof {
            key: B256::random_with(&mut rng),
            proof: InnerStorageSlotProof::NonExisting {
                left_neighbor: NeighborStorageSlotProofEntry {
                    inner: random_entry(&mut rng),
                    leaf_key: B256::random_with(&mut rng),
                },
                right_neighbor: NeighborStorageSlotProofEntry {
                    inner: random_entry(&mut rng),
                    leaf_key: B256::random_with(&mut rng),
                },
            },
        };

        insta::assert_yaml_snapshot!("non_existing_proof", proof);
    }
}
