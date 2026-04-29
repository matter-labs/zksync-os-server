use std::collections::{BTreeMap, HashMap};
use zk_ee::utils::Bytes32;
use zk_os_basic_system::system_implementation::flat_storage_model::FlatStorageLeaf;
use zk_os_basic_system_dev::system_implementation::flat_storage_model::FlatStorageLeaf as FlatStorageLeafDev;
use zk_os_forward_system::run::{LeafProof, ReadStorage, ReadStorageTree};
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_merkle_tree::{Blake2Hasher, HashTree, Leaf, TreeOperation};

const TREE_DEPTH: u8 = 64;

#[derive(Debug)]
pub(super) struct TreeAdapter {
    final_leaf_count: u64,
    sorted_leaves: BTreeMap<u64, Leaf>,
    key_to_index: HashMap<Bytes32, u64>,
    missing_key_to_prev_index: HashMap<Bytes32, u64>,
    sibling_hashes: HashMap<(u8, u64), Bytes32>,
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
                hash.as_u8_array().into()
            };

            idx_on_level /= 2;
            last_idx_on_level /= 2;
        }
        Box::new(path)
    }
}

impl ReadStorage for TreeAdapter {
    fn read(&mut self, key: Bytes32) -> Option<Bytes32> {
        if let Some(idx) = self.key_to_index.get(&key) {
            let leaf = self.sorted_leaves.get(idx).unwrap_or_else(|| {
                panic!("requested Merkle proof for unexpected index: {idx}");
            });
            Some(leaf.value.0.into())
        } else {
            assert!(
                self.missing_key_to_prev_index.contains_key(&key),
                "requested read of unexpected key: {key:?}"
            );
            None
        }
    }
}

impl zk_os_forward_system_dev::run::ReadStorage for TreeAdapter {
    fn read(&mut self, key: zk_ee_dev::utils::Bytes32) -> Option<zk_ee_dev::utils::Bytes32> {
        <Self as ReadStorage>::read(self, key.as_u8_array().into())
            .map(|value| value.as_u8_array().into())
    }
}

impl ReadStorageTree for TreeAdapter {
    fn tree_index(&mut self, key: Bytes32) -> Option<u64> {
        if let Some(idx) = self.key_to_index.get(&key) {
            return Some(*idx);
        }
        assert!(
            self.missing_key_to_prev_index.contains_key(&key),
            "requested index for unexpected key: {key:?}"
        );
        None
    }

    fn merkle_proof(&mut self, tree_index: u64) -> LeafProof {
        let leaf = self.sorted_leaves.get(&tree_index).unwrap_or_else(|| {
            panic!("requested Merkle proof for unexpected index: {tree_index}");
        });
        let leaf = FlatStorageLeaf {
            key: leaf.key.0.into(),
            value: leaf.value.0.into(),
            next: leaf.next_index,
        };
        let merkle_path = self.merkle_path(tree_index);
        LeafProof::new(tree_index, leaf, merkle_path)
    }

    fn prev_tree_index(&mut self, key: Bytes32) -> u64 {
        self.missing_key_to_prev_index
            .get(&key)
            .copied()
            .unwrap_or_else(|| {
                panic!("requested prev index for unexpected key: {key:?}");
            })
    }
}

impl zk_os_forward_system_dev::run::ReadStorageTree for TreeAdapter {
    fn tree_index(&mut self, key: zk_ee_dev::utils::Bytes32) -> Option<u64> {
        <Self as ReadStorageTree>::tree_index(self, key.as_u8_array().into())
    }

    fn merkle_proof(&mut self, tree_index: u64) -> zk_os_forward_system_dev::run::LeafProof {
        let leaf = self.sorted_leaves.get(&tree_index).unwrap_or_else(|| {
            panic!("requested Merkle proof for unexpected index: {tree_index}");
        });
        let leaf = FlatStorageLeafDev {
            key: leaf.key.0.into(),
            value: leaf.value.0.into(),
            next: leaf.next_index,
        };
        let merkle_path = self.merkle_path(tree_index);
        zk_os_forward_system_dev::run::LeafProof::new(tree_index, leaf, merkle_path)
    }

    fn prev_tree_index(&mut self, key: zk_ee_dev::utils::Bytes32) -> u64 {
        <Self as ReadStorageTree>::prev_tree_index(self, key.as_u8_array().into())
    }
}
