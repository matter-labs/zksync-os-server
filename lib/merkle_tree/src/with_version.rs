use crate::{
    Database, DefaultTreeParams, HashTree, MerkleTree, RocksDBWrapper, TreeParams, leaf_nibbles,
    types::{KeyLookup, Leaf, Node, NodeKey},
};
use alloy::primitives::{B256, FixedBytes};
use zk_ee::utils::Bytes32;
use zk_ee_0_1_0::utils::Bytes32 as Bytes32V0_1_0;
use zk_os_basic_system::system_implementation::flat_storage_model::FlatStorageLeaf;
use zk_os_basic_system_0_1_0::system_implementation::flat_storage_model::FlatStorageLeaf as FlatStorageLeafV0_1_0;
use zk_os_forward_system::run::{LeafProof, ReadStorage, ReadStorageTree};
use zk_os_forward_system_0_1_0::run::{
    LeafProof as LeafProofV0_1_0, ReadStorage as ReadStorageV0_1_0,
    ReadStorageTree as ReadStorageTreeV0_1_0,
};

pub struct MerkleTreeVersion<DB: Database = RocksDBWrapper, P: TreeParams = DefaultTreeParams> {
    pub tree: MerkleTree<DB, P>,
    pub block: u64,
}

impl<DB: Database, P: TreeParams> MerkleTreeVersion<DB, P> {
    pub fn root_info(&self) -> Result<(B256, u64), anyhow::Error> {
        // We know that the root exists, as some version was loaded into the tree already.
        self.tree.root_info(self.block).transpose().unwrap()
    }

    fn traverse_to_leaf(&mut self, tree_index: u64) -> Option<Leaf> {
        let mut current_node = self
            .tree
            .db()
            .try_root(self.block)
            .unwrap()
            .unwrap()
            .root_node;

        let mut nibble_count = 1;
        loop {
            let index_on_level =
                tree_index >> ((leaf_nibbles::<P>() - nibble_count) * P::INTERNAL_NODE_DEPTH);
            let child_index = index_on_level as usize % (1 << P::INTERNAL_NODE_DEPTH);

            let Some(child) = current_node.children.get(child_index) else {
                break None;
            };
            current_node = match self
                .tree
                .db
                .try_nodes(&[NodeKey {
                    version: child.version,
                    nibble_count,
                    index_on_level,
                }])
                .expect("inconsistent child reference")[0]
                .clone()
            {
                Node::Internal(internal) => internal,
                Node::Leaf(leaf) => break Some(leaf),
            };
            nibble_count += 1;
        }
    }
}

impl<DB: Database + 'static, P: TreeParams + 'static> ReadStorage for MerkleTreeVersion<DB, P> {
    fn read(&mut self, key: Bytes32) -> Option<Bytes32> {
        <Self as ReadStorageTree>::tree_index(self, key).and_then(|index| {
            self.traverse_to_leaf(index)
                .map(|Leaf { value, .. }| fixed_bytes_to_bytes32(value))
        })
    }
}

impl<DB: Database + 'static, P: TreeParams + 'static> ReadStorageV0_1_0
    for MerkleTreeVersion<DB, P>
{
    fn read(&mut self, key: Bytes32V0_1_0) -> Option<Bytes32V0_1_0> {
        <Self as ReadStorage>::read(self, key.as_u8_array().into()).map(|v| v.as_u8_array().into())
    }
}

impl<DB: Database + 'static, P: TreeParams + 'static> ReadStorageTree for MerkleTreeVersion<DB, P> {
    fn tree_index(&mut self, key: Bytes32) -> Option<u64> {
        self.tree
            .db()
            .indices(self.block, &[FixedBytes::from_slice(key.as_u8_ref())])
            .ok()
            .and_then(|v| match v[0] {
                KeyLookup::Existing(x) => Some(x),
                KeyLookup::Missing { .. } => None,
            })
    }

    fn merkle_proof(&mut self, tree_index: u64) -> LeafProof {
        let mut sibling_hashes = Box::new([Bytes32::zero(); 64]);

        let mut current_node = self
            .tree
            .db()
            .try_root(self.block)
            .unwrap()
            .unwrap()
            .root_node;

        let mut i = P::TREE_DEPTH as usize;
        let mut nibble_count = 1;
        let leaf = loop {
            let index_on_level =
                tree_index >> ((leaf_nibbles::<P>() - nibble_count) * P::INTERNAL_NODE_DEPTH);
            let child_index = index_on_level as usize % (1 << P::INTERNAL_NODE_DEPTH);

            // the root does not contain any nodes apart from its children
            if nibble_count > 1 {
                let hashes = current_node
                    .internal_hashes::<P>(&self.tree.hasher, i as u8 - 3)
                    .0;

                for depth in 0..P::INTERNAL_NODE_DEPTH - 1 {
                    let needed_for_this_and_lower_levels = (2 << (depth + 1)) - 2;
                    let needed_for_all = (2 << (P::INTERNAL_NODE_DEPTH - 1)) - 2;
                    let skip = needed_for_all - needed_for_this_and_lower_levels;

                    let index = child_index >> (P::INTERNAL_NODE_DEPTH - depth - 1);

                    i -= 1;
                    sibling_hashes[i] = hashes[skip + (index ^ 1)].0.into();
                }
            }

            i -= 1;
            sibling_hashes[i] = current_node
                .children
                .get(child_index ^ 1)
                .map(|x| x.hash)
                .unwrap_or(self.tree.hasher.empty_subtree_hash(i as u8))
                .0
                .into();

            let Some(child) = current_node.children.get(child_index) else {
                break Leaf::default();
            };
            current_node = match self
                .tree
                .db
                .try_nodes(&[NodeKey {
                    version: child.version,
                    nibble_count,
                    index_on_level,
                }])
                .expect("inconsistent child reference")[0]
                .clone()
            {
                Node::Internal(internal) => internal,
                Node::Leaf(leaf) => break leaf,
            };
            nibble_count += 1;
        };

        for i in 0..i {
            sibling_hashes[i] = self.tree.hasher.empty_subtree_hash(i as u8).0.into();
        }

        LeafProof::new(
            tree_index,
            FlatStorageLeaf {
                key: leaf.key.0.into(),
                value: leaf.value.0.into(),
                next: leaf.next_index,
            },
            sibling_hashes,
        )
    }

    fn prev_tree_index(&mut self, key: Bytes32) -> u64 {
        // TODO this will fail for existing nodes
        let res = &self
            .tree
            .db()
            .indices(self.block, &[FixedBytes::from_slice(key.as_u8_ref())])
            .unwrap()[0];
        match res {
            KeyLookup::Existing(_) => todo!(),
            KeyLookup::Missing {
                prev_key_and_index: (_, index),
                ..
            } => *index,
        }
    }
}

impl<DB: Database + 'static, P: TreeParams + 'static> ReadStorageTreeV0_1_0
    for MerkleTreeVersion<DB, P>
{
    fn tree_index(&mut self, key: Bytes32V0_1_0) -> Option<u64> {
        <Self as ReadStorageTree>::tree_index(self, key.as_u8_array().into())
    }

    fn merkle_proof(&mut self, tree_index: u64) -> LeafProofV0_1_0 {
        let proof = <Self as ReadStorageTree>::merkle_proof(self, tree_index);
        let mut path = Box::new([Bytes32V0_1_0::zero(); 64]);

        for i in 0..64 {
            path[i] = proof.path[i].as_u8_array().into();
        }

        LeafProofV0_1_0::new(
            proof.index,
            FlatStorageLeafV0_1_0 {
                key: proof.leaf.key.as_u8_array().into(),
                value: proof.leaf.value.as_u8_array().into(),
                next: proof.leaf.next,
            },
            path,
        )
    }

    fn prev_tree_index(&mut self, key: Bytes32V0_1_0) -> u64 {
        <Self as ReadStorageTree>::prev_tree_index(self, key.as_u8_array().into())
    }
}

pub fn fixed_bytes_to_bytes32(x: B256) -> Bytes32 {
    let x: [u8; 32] = x.into();
    x.into()
}

impl<DB: Database + Clone, P: TreeParams> Clone for MerkleTreeVersion<DB, P>
where
    P::Hasher: Clone,
{
    fn clone(&self) -> Self {
        Self {
            tree: self.tree.clone(),
            block: self.block,
        }
    }
}
