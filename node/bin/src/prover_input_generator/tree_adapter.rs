use alloy::primitives::B256;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::thread;
use zk_ee::utils::Bytes32;
use zk_os_basic_system::system_implementation::flat_storage_model::FlatStorageLeaf;
use zk_os_basic_system_prev::system_implementation::flat_storage_model::FlatStorageLeaf as FlatStorageLeafDev;
use zk_os_forward_system::run::{LeafProof, ReadStorage, ReadStorageTree};
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_merkle_tree::{Blake2Hasher, HashTree, Leaf, TreeOperation};

// `VersionedMerkleTree` now lives in its own lib crate so the witness-assembly
// code can reuse it without depending on `node/bin`. Re-export it here to keep
// the existing `tree_adapter::VersionedMerkleTree` path valid for callers.
pub(crate) use versioned_merkle_tree::VersionedMerkleTree;

const TREE_DEPTH: u8 = 64;

/// Error returned by [`TreeOutputAdapter`] when it contains no queried data.
#[derive(Debug)]
struct NoData;

/// Very fast [`ReadStorage`] / [`ReadStorageTree`] implementation based on the data contained in a Merkle tree batch update proof.
/// It requires zero I/O for all operations, but doesn't work for storage slots not read / written during block execution.
#[derive(Debug)]
pub(super) struct TreeOutputAdapter {
    leaf_count_before_update: u64,
    sorted_leaves: BTreeMap<u64, Leaf>,
    key_to_op: HashMap<B256, TreeOperation>,
    sibling_hashes: HashMap<(u8, u64), B256>,
    // Fields below are only used for metrics and can be removed in the future.
    queried_proofs: HashSet<u64>,
}

impl Drop for TreeOutputAdapter {
    fn drop(&mut self) {
        const DEBUG_COUNT: usize = 5;

        if thread::panicking() {
            return; // Do not report potentially incomplete data if generating prover input failed
        }

        let expected_queried_proofs: HashSet<_> = self.sorted_leaves.keys().copied().collect();
        let unexpected_queried_proofs: Vec<_> = self
            .queried_proofs
            .difference(&expected_queried_proofs)
            .take(DEBUG_COUNT)
            .collect();
        if !unexpected_queried_proofs.is_empty() {
            tracing::warn!(
                ?unexpected_queried_proofs,
                "prover queried unexpected Merkle proofs"
            );
        }

        let not_queried_proofs: Vec<_> = expected_queried_proofs
            .difference(&self.queried_proofs)
            .take(DEBUG_COUNT)
            .collect();
        if !not_queried_proofs.is_empty() {
            tracing::warn!(
                ?not_queried_proofs,
                "prover didn't query expected Merkle proofs"
            );
        }
    }
}

impl TreeOutputAdapter {
    pub(super) fn new(tree_data: BlockMerkleTreeData) -> Self {
        let key_to_index: HashMap<_, _> = tree_data.keys_and_ops().collect();
        let sibling_hashes = tree_data
            .proof
            .sibling_hashes(TREE_DEPTH, tree_data.input.leaf_count)
            .map(|(location, hash)| (location, hash.0.into()))
            .collect();

        Self {
            queried_proofs: HashSet::with_capacity(tree_data.proof.sorted_leaves.len()),
            leaf_count_before_update: tree_data.input.leaf_count,
            sorted_leaves: tree_data.proof.sorted_leaves,
            key_to_op: key_to_index,
            sibling_hashes,
        }
    }

    pub(super) fn with_fallback(self, fallback: VersionedMerkleTree) -> EfficientTreeAdapter {
        EfficientTreeAdapter {
            main: self,
            fallback,
        }
    }

    fn merkle_path<B>(&self, tree_index: u64) -> Box<[B; 64]>
    where
        B: Default + Copy + From<[u8; 32]>,
    {
        let mut path = [B::default(); TREE_DEPTH as usize];
        let mut idx_on_level = tree_index;
        let mut last_idx_on_level = self.leaf_count_before_update - 1;
        for (depth, sibling_hash) in (0..TREE_DEPTH).zip(&mut path) {
            let sibling_location = (depth, idx_on_level ^ 1);
            let hash = self.sibling_hashes.get(&sibling_location).copied();
            let hash = hash.unwrap_or_else(|| {
                // The hash may only be missing for the last index on the level.
                assert!(
                    sibling_location.1 > last_idx_on_level,
                    "missing Merkle path for index {tree_index} at {sibling_location:?}"
                );
                Blake2Hasher.empty_subtree_hash(depth)
            });
            *sibling_hash = hash.0.into();

            idx_on_level /= 2;
            last_idx_on_level /= 2;
        }
        Box::new(path)
    }

    fn read(&mut self, key: B256) -> Result<Option<B256>, NoData> {
        let op = self.key_to_op.get(&key).ok_or(NoData)?;
        Ok(match op {
            TreeOperation::Hit { index } => {
                let leaf = self.sorted_leaves.get(index).ok_or(NoData)?;
                Some(leaf.value)
            }
            TreeOperation::Miss { .. } => None,
        })
    }

    fn tree_index(&mut self, key: B256) -> Result<Option<u64>, NoData> {
        let op = self.key_to_op.get(&key).ok_or(NoData)?;
        Ok(match op {
            TreeOperation::Hit { index } => Some(*index),
            TreeOperation::Miss { .. } => None,
        })
    }

    fn merkle_proof(&mut self, tree_index: u64) -> Result<LeafProof, NoData> {
        self.queried_proofs.insert(tree_index);

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
        let op = self.key_to_op.get(&key).ok_or(NoData)?;
        if let TreeOperation::Miss { prev_index } = op {
            Ok(*prev_index)
        } else {
            Err(NoData)
        }
    }
}

/// Efficient tree adapter that takes most data from [`TreeOutputAdapter`] and uses [`VersionedMerkleTree`]
/// as a fallback. During normal execution, the fallback shouldn't be used at all.
#[derive(Debug)]
pub(super) struct EfficientTreeAdapter {
    main: TreeOutputAdapter,
    fallback: VersionedMerkleTree,
}

impl EfficientTreeAdapter {
    fn read_inner(&mut self, key: B256) -> Option<B256> {
        self.main
            .read(key)
            .unwrap_or_else(|_| self.fallback.read(key))
    }

    fn tree_index_inner(&mut self, key: B256) -> Option<u64> {
        self.main
            .tree_index(key)
            .unwrap_or_else(|_| self.fallback.tree_index(key))
    }

    fn merkle_proof_inner(&mut self, tree_index: u64) -> LeafProof {
        self.main
            .merkle_proof(tree_index)
            .unwrap_or_else(|_| self.fallback.merkle_proof(tree_index))
    }

    fn prev_tree_index_inner(&mut self, key: B256) -> u64 {
        self.main
            .prev_tree_index(key)
            .unwrap_or_else(|_| self.fallback.prev_tree_index(key))
    }
}

impl ReadStorage for EfficientTreeAdapter {
    fn read(&mut self, key: Bytes32) -> Option<Bytes32> {
        self.read_inner(key.as_u8_array().into())
            .map(|value| value.0.into())
    }
}

impl zk_os_forward_system_prev::run::ReadStorage for EfficientTreeAdapter {
    fn read(&mut self, key: zk_ee_prev::utils::Bytes32) -> Option<zk_ee_prev::utils::Bytes32> {
        self.read_inner(key.as_u8_array().into())
            .map(|value| value.0.into())
    }
}

impl ReadStorageTree for EfficientTreeAdapter {
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

impl zk_os_forward_system_prev::run::ReadStorageTree for EfficientTreeAdapter {
    fn tree_index(&mut self, key: zk_ee_prev::utils::Bytes32) -> Option<u64> {
        self.tree_index_inner(key.as_u8_array().into())
    }

    fn merkle_proof(&mut self, tree_index: u64) -> zk_os_forward_system_prev::run::LeafProof {
        let LeafProof {
            index, leaf, path, ..
        } = self.merkle_proof_inner(tree_index);
        let leaf = FlatStorageLeafDev {
            key: leaf.key.as_u8_array().into(),
            value: leaf.value.as_u8_array().into(),
            next: leaf.next,
        };
        let path = Box::new(path.map(|hash| hash.as_u8_array().into()));
        zk_os_forward_system_prev::run::LeafProof::new(index, leaf, path)
    }

    fn prev_tree_index(&mut self, key: zk_ee_prev::utils::Bytes32) -> u64 {
        self.prev_tree_index_inner(key.as_u8_array().into())
    }
}
