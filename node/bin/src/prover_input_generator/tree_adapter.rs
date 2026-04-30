#![allow(dead_code)] // FIXME

use alloy::primitives::B256;
use std::collections::{BTreeMap, HashMap};
use std::thread;
use zk_ee::utils::Bytes32;
use zk_os_basic_system::system_implementation::flat_storage_model::FlatStorageLeaf;
use zk_os_basic_system_dev::system_implementation::flat_storage_model::FlatStorageLeaf as FlatStorageLeafDev;
use zk_os_forward_system::run::{LeafProof, ReadStorage, ReadStorageTree};
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_merkle_tree::{
    Blake2Hasher, HashTree, Leaf, MerkleTree, MerkleTreeProver, RocksDBWrapper, TreeOperation,
    api::flat,
};

const TREE_DEPTH: u8 = 64;

/// Error returned by [`TreeAdapter`] when it contains no queried data.
#[derive(Debug)]
struct NoData;

#[derive(Debug)]
pub(super) struct TreeAdapter {
    final_leaf_count: u64,
    sorted_leaves: BTreeMap<u64, Leaf>,
    key_to_index: HashMap<B256, u64>,
    missing_key_to_prev_index: HashMap<B256, u64>,
    sibling_hashes: HashMap<(u8, u64), B256>,
}

impl TreeAdapter {
    pub(super) fn new(tree_data: BlockMerkleTreeData) -> Self {
        let key_to_index = tree_data
            .proof
            .sorted_leaves
            .iter()
            .map(|(index, leaf)| (leaf.key.0.into(), *index))
            .collect();

        let missing_key_to_prev_index = tree_data
            .keys_and_ops()
            .filter_map(|(key, op)| {
                let TreeOperation::Miss { prev_index } = op else {
                    return None;
                };
                Some((key, prev_index))
            })
            .collect();

        let sibling_hashes = tree_data
            .proof
            .sibling_hashes(TREE_DEPTH, tree_data.output.leaf_count)
            .map(|(location, hash)| (location, hash.0.into()))
            .collect();

        Self {
            final_leaf_count: tree_data.output.leaf_count,
            sorted_leaves: tree_data.proof.sorted_leaves,
            key_to_index,
            missing_key_to_prev_index,
            sibling_hashes,
        }
    }

    fn merkle_path<B>(&self, tree_index: u64) -> Box<[B; 64]>
    where
        B: Default + Copy + From<[u8; 32]>,
    {
        let mut path = [B::default(); TREE_DEPTH as usize];
        let mut idx_on_level = tree_index;
        let mut last_idx_on_level = self.final_leaf_count - 1;
        for (depth, sibling_hash) in (0..TREE_DEPTH).zip(&mut path) {
            *sibling_hash = if idx_on_level == last_idx_on_level {
                Blake2Hasher.empty_subtree_hash(depth).0.into()
            } else {
                let sibling_location = (depth, idx_on_level ^ 1);
                let hash = self
                    .sibling_hashes
                    .get(&sibling_location)
                    .unwrap_or_else(|| {
                        panic!(
                            "missing Merkle path for index {tree_index} at {sibling_location:?}"
                        );
                    });
                hash.0.into()
            };

            idx_on_level /= 2;
            last_idx_on_level /= 2;
        }
        Box::new(path)
    }

    fn read(&mut self, key: B256) -> Result<Option<B256>, NoData> {
        Ok(if let Some(idx) = self.key_to_index.get(&key) {
            let leaf = self.sorted_leaves.get(idx).ok_or(NoData)?;
            Some(leaf.value)
        } else if self.missing_key_to_prev_index.contains_key(&key) {
            None
        } else {
            return Err(NoData);
        })
    }

    fn tree_index(&mut self, key: B256) -> Result<Option<u64>, NoData> {
        if let Some(idx) = self.key_to_index.get(&key) {
            Ok(Some(*idx))
        } else if self.missing_key_to_prev_index.contains_key(&key) {
            Ok(None)
        } else {
            Err(NoData)
        }
    }

    fn merkle_proof(&mut self, tree_index: u64) -> Result<LeafProof, NoData> {
        let leaf = self.sorted_leaves.get(&tree_index).ok_or(NoData)?;
        let leaf = FlatStorageLeaf {
            key: leaf.key.0.into(),
            value: leaf.value.0.into(),
            next: leaf.next_index,
        };
        let merkle_path = self.merkle_path(tree_index);
        Ok(LeafProof::new(tree_index, leaf, merkle_path))
    }

    fn prev_tree_index(&mut self, key: B256) -> Result<u64, NoData> {
        self.missing_key_to_prev_index
            .get(&key)
            .copied()
            .ok_or(NoData)
    }
}

/// Storage adapter that reads data from the Merkle tree. This adapter is very inefficient in terms of I/O,
/// but is universal as opposed to using a batch update proof (which will miss data for any keys
/// not read / written in the batch).
#[derive(Debug)]
pub(super) struct VersionedMerkleTree {
    inner: MerkleTree<RocksDBWrapper>,
    version: u64,
    cached_key_to_index: HashMap<B256, Option<u64>>,
    cached_missing_key_to_prev_index: HashMap<B256, u64>,
    cached_proofs: HashMap<u64, flat::StorageSlotProofEntryWithKey>,
}

impl VersionedMerkleTree {
    pub(super) fn new(inner: MerkleTree<RocksDBWrapper>, version: u64) -> Self {
        Self {
            inner,
            version,
            cached_key_to_index: HashMap::new(),
            cached_missing_key_to_prev_index: HashMap::new(),
            cached_proofs: HashMap::new(),
        }
    }

    fn read_inner(&mut self, key: B256) -> Option<B256> {
        let (proof, _) = self
            .inner
            .prove_flat(self.version, &[key])
            .expect("failed getting Merkle proof")
            .expect("tree version disappeared");
        assert_eq!(
            proof.len(),
            1,
            "sanity check failed: unexpected proof length"
        );
        let proof = proof.into_iter().next().unwrap();
        let value = proof.value();

        // Cache the proof since it's guaranteed to be requested later.
        self.cache_proof(proof);

        value
    }

    fn cache_proof(&mut self, proof: flat::StorageSlotProof) {
        match proof.proof {
            flat::InnerStorageSlotProof::Existing(entry) => {
                self.insert_proof(proof.key, entry);
            }
            flat::InnerStorageSlotProof::NonExisting {
                left_neighbor,
                right_neighbor,
            } => {
                self.cached_key_to_index.insert(proof.key, None);
                self.cached_missing_key_to_prev_index
                    .insert(proof.key, left_neighbor.inner.index);
                self.insert_proof(left_neighbor.leaf_key, left_neighbor.inner);
                self.insert_proof(right_neighbor.leaf_key, right_neighbor.inner);
            }
        }
    }

    fn insert_proof(&mut self, key: B256, proof: flat::StorageSlotProofEntry) {
        self.cached_key_to_index.insert(key, Some(proof.index));
        self.cached_proofs.insert(
            proof.index,
            flat::StorageSlotProofEntryWithKey {
                inner: proof,
                leaf_key: key,
            },
        );
    }

    fn tree_index_inner(&mut self, key: B256) -> Option<u64> {
        if !self.cached_key_to_index.contains_key(&key) {
            // Use proof API to get the necessary data. This is inefficient, but should (almost) never
            // be triggered in practice.
            self.read_inner(key);
        }
        self.cached_key_to_index[&key]
    }

    fn merkle_proof_inner(&mut self, tree_index: u64) -> LeafProof {
        dbg!(tree_index);
        if !self.cached_proofs.contains_key(&tree_index) {
            let proof = self
                .inner
                .prove_index_flat(self.version, tree_index)
                .expect("failed getting Merkle proof")
                .expect("tree version disappeared");
            self.cached_proofs.insert(tree_index, proof);
        }
        Self::map_proof(&self.cached_proofs[&tree_index])
    }

    fn map_proof(proof: &flat::StorageSlotProofEntryWithKey) -> LeafProof {
        let leaf = FlatStorageLeaf {
            key: proof.leaf_key.0.into(),
            value: proof.inner.value.0.into(),
            next: proof.inner.next_index,
        };

        let mut merkle_path = Box::new([Bytes32::default(); 64]);
        for (i, hash) in proof.inner.siblings.iter().enumerate() {
            merkle_path[i] = hash.0.into();
        }
        // Fill in remaining Merkle path hashes from empty subtree hashes.
        let merkle_path_len = proof.inner.siblings.len() as u8;
        for level in merkle_path_len..TREE_DEPTH {
            merkle_path[usize::from(level)] = Blake2Hasher.empty_subtree_hash(level).0.into();
        }

        LeafProof::new(proof.inner.index, leaf, merkle_path)
    }

    fn prev_tree_index_inner(&mut self, key: B256) -> u64 {
        if !self.cached_missing_key_to_prev_index.contains_key(&key) {
            assert_eq!(self.read_inner(key), None);
        }
        self.cached_missing_key_to_prev_index[&key]
    }
}

/// Reports storage-related metrics on drop.
impl Drop for VersionedMerkleTree {
    fn drop(&mut self) {
        if thread::panicking() {
            return; // Do not report potentially incomplete data if generating prover input failed
        }

        tracing::info!(
            version = self.version,
            cached_key_to_index.len = self.cached_key_to_index.len(),
            cached_missing_key_to_prev_index.len = self.cached_missing_key_to_prev_index.len(),
            cached_proofs.len = self.cached_proofs.len(),
            "finished providing storage via Merkle tree"
        );
    }
}

impl ReadStorage for VersionedMerkleTree {
    fn read(&mut self, key: Bytes32) -> Option<Bytes32> {
        self.read_inner(key.as_u8_array().into())
            .map(|value| value.0.into())
    }
}

impl zk_os_forward_system_dev::run::ReadStorage for VersionedMerkleTree {
    fn read(&mut self, key: zk_ee_dev::utils::Bytes32) -> Option<zk_ee_dev::utils::Bytes32> {
        self.read_inner(key.as_u8_array().into())
            .map(|value| value.0.into())
    }
}

impl ReadStorageTree for VersionedMerkleTree {
    fn tree_index(&mut self, key: Bytes32) -> Option<u64> {
        self.tree_index_inner(key.as_u8_array().into())
    }

    fn merkle_proof(&mut self, tree_index: u64) -> LeafProof {
        self.merkle_proof_inner(tree_index)
    }

    fn prev_tree_index(&mut self, key: Bytes32) -> u64 {
        self.prev_tree_index_inner(key.as_u8_array().into())
    }
}

impl zk_os_forward_system_dev::run::ReadStorageTree for VersionedMerkleTree {
    fn tree_index(&mut self, key: zk_ee_dev::utils::Bytes32) -> Option<u64> {
        self.tree_index_inner(key.as_u8_array().into())
    }

    fn merkle_proof(&mut self, tree_index: u64) -> zk_os_forward_system_dev::run::LeafProof {
        let LeafProof {
            index, leaf, path, ..
        } = self.merkle_proof_inner(tree_index);
        let leaf = FlatStorageLeafDev {
            key: leaf.key.as_u8_array().into(),
            value: leaf.value.as_u8_array().into(),
            next: leaf.next,
        };
        let path = Box::new(path.map(|hash| hash.as_u8_array().into()));
        zk_os_forward_system_dev::run::LeafProof::new(index, leaf, path)
    }

    fn prev_tree_index(&mut self, key: zk_ee_dev::utils::Bytes32) -> u64 {
        self.prev_tree_index_inner(key.as_u8_array().into())
    }
}
