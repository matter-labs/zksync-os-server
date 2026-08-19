use zk_ee_prev::utils::Bytes32;
use zk_os_basic_system_0_2_10::system_implementation::flat_storage_model::FlatStorageLeaf as FlatStorageLeafV6;
use zk_os_basic_system_prev::system_implementation::flat_storage_model::FlatStorageLeaf;
use zk_os_forward_system_prev::run::{LeafProof, ReadStorage, ReadStorageTree};
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_native_pig::tree::{EfficientTreeAdapter, RawLeafProof};

pub(super) use zksync_os_native_pig::tree::VersionedMerkleTree;

/// [`EfficientTreeAdapter`] wrapper implementing the V6 and V7 lanes' zksync-os storage traits.
#[derive(Debug)]
pub(super) struct LaneTreeAdapter(EfficientTreeAdapter);

impl LaneTreeAdapter {
    pub(super) fn new(tree_data: BlockMerkleTreeData, fallback: VersionedMerkleTree) -> Self {
        Self(EfficientTreeAdapter::new(tree_data, fallback))
    }
}

impl ReadStorage for LaneTreeAdapter {
    fn read(&mut self, key: Bytes32) -> Option<Bytes32> {
        self.0
            .read(key.as_u8_array().into())
            .map(|value| value.0.into())
    }
}

impl ReadStorageTree for LaneTreeAdapter {
    fn tree_index(&mut self, key: Bytes32) -> Option<u64> {
        self.0.tree_index(key.as_u8_array().into())
    }

    fn merkle_proof(&mut self, tree_index: u64) -> LeafProof {
        let proof = self.0.merkle_proof(tree_index);
        let leaf = FlatStorageLeaf {
            key: proof.key.0.into(),
            value: proof.value.0.into(),
            next: proof.next_index,
        };
        LeafProof::new(proof.index, leaf, map_path(&proof))
    }

    fn prev_tree_index(&mut self, key: Bytes32) -> u64 {
        self.0.prev_tree_index(key.as_u8_array().into())
    }
}

impl zk_os_forward_system_0_2_10::run::ReadStorage for LaneTreeAdapter {
    fn read(&mut self, key: zk_ee_0_2_10::utils::Bytes32) -> Option<zk_ee_0_2_10::utils::Bytes32> {
        self.0
            .read(key.as_u8_array().into())
            .map(|value| value.0.into())
    }
}

impl zk_os_forward_system_0_2_10::run::ReadStorageTree for LaneTreeAdapter {
    fn tree_index(&mut self, key: zk_ee_0_2_10::utils::Bytes32) -> Option<u64> {
        self.0.tree_index(key.as_u8_array().into())
    }

    fn merkle_proof(&mut self, tree_index: u64) -> zk_os_forward_system_0_2_10::run::LeafProof {
        let proof = self.0.merkle_proof(tree_index);
        let leaf = FlatStorageLeafV6 {
            key: proof.key.0.into(),
            value: proof.value.0.into(),
            next: proof.next_index,
        };
        zk_os_forward_system_0_2_10::run::LeafProof::new(proof.index, leaf, map_path(&proof))
    }

    fn prev_tree_index(&mut self, key: zk_ee_0_2_10::utils::Bytes32) -> u64 {
        self.0.prev_tree_index(key.as_u8_array().into())
    }
}

fn map_path<B>(proof: &RawLeafProof) -> Box<[B; 64]>
where
    B: Default + Copy + From<[u8; 32]>,
{
    let mut path = Box::new([B::default(); 64]);
    for (slot, hash) in path.iter_mut().zip(proof.path.iter()) {
        *slot = hash.0.into();
    }
    path
}
