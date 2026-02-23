//! Merkle tree-related types suitable for use in RPC.

use std::collections::BTreeMap;

use alloy::primitives::{Address, B256, U32, U64};
use serde::{Deserialize, Serialize};

use crate::{BatchTreeProof, Blake2Hasher, HashTree, Leaf, TreeOperation};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateCommitmentPreimage {
    pub next_free_slot: U64,
    pub block_number: U32,
    pub last_256_block_hashes_blake: B256,
    pub last_block_timestamp: U64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSlotProofEntry {
    pub index: u64,
    pub value: B256,
    pub next_index: u64,
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
            hash = if index % 2 == 0 {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InnerStorageSlotProof {
    Existing(StorageSlotProofEntry),
    NonExisting {
        left_neighbor: NeighborStorageSlotProofEntry,
        right_neighbor: NeighborStorageSlotProofEntry,
    },
}

impl InnerStorageSlotProof {
    pub(crate) fn verify(&self, tree_depth: u8, key: B256) -> anyhow::Result<B256> {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSlotProof {
    pub key: B256,
    pub proof: InnerStorageSlotProof,
}

impl StorageSlotProof {
    /// Verifies the internal consistency of this proof and returns the recovered tree root hash.
    pub fn verify(&self, tree_depth: u8) -> anyhow::Result<B256> {
        self.proof.verify(tree_depth, self.key)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchStorageProof {
    pub address: Address,
    pub state_commitment_preimage: StateCommitmentPreimage,
    pub storage_proofs: Vec<StorageSlotProof>,
}

impl BatchTreeProof {
    /// Converts this proof to the API format by filling Merkle paths that are implicitly present
    /// in the proof.
    pub(crate) fn to_api(
        &self,
        tree_depth: u8,
        leaf_count: u64,
    ) -> impl Iterator<Item = InnerStorageSlotProof> {
        assert!(self.operations.is_empty());

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
                return current.value;
            }
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
