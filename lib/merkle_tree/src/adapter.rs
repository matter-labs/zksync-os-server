use std::iter::once;

use crate::{
    leaf_nibbles,
    types::{KeyLookup, Leaf, Node, NodeKey},
    Database, MerkleTree, TreeParams,
};
use alloy::primitives::{FixedBytes, B256};
use zk_ee::{kv_markers::UsizeDeserializable, utils::Bytes32};
use zk_os_basic_system::system_implementation::flat_storage_model::FlatStorageLeaf;
use zk_os_forward_system::run::{LeafProof, ReadStorage, ReadStorageTree};

pub struct MerkleTreeVersion<DB: Database, P: TreeParams> {
    tree: MerkleTree<DB, P>,
    version: u64,
}

impl<DB: Database, P: TreeParams> MerkleTreeVersion<DB, P> {
    fn read_leaf(&mut self, index: u64) -> Option<Leaf> {
        self.tree
            .db
            .try_nodes(&[NodeKey {
                version: self.version,
                nibble_count: leaf_nibbles::<P>(),
                index_on_level: index,
            }])
            .ok()
            .map(|n| match n[0] {
                Node::Internal(_) => unreachable!(),
                Node::Leaf(leaf) => leaf,
            })
    }
}

impl<DB: Database + 'static, P: TreeParams + 'static> ReadStorage for MerkleTreeVersion<DB, P> {
    fn read(&mut self, key: Bytes32) -> Option<Bytes32> {
        self.tree_index(key).and_then(|index| {
            self.read_leaf(index)
                .map(|Leaf { value, .. }| fixed_bytes_to_bytes32(value))
        })
    }
}

impl<DB: Database + 'static, P: TreeParams + 'static> ReadStorageTree for MerkleTreeVersion<DB, P> {
    fn tree_index(&mut self, key: Bytes32) -> Option<u64> {
        self.tree
            .db
            .indices(self.version, &[FixedBytes::from_slice(key.as_u8_ref())])
            .ok()
            .map(|v| match v[0] {
                KeyLookup::Existing(x) => x,
                KeyLookup::Missing { .. } => panic!("checking index of nonexistent key"),
            })
    }

    fn merkle_proof(&mut self, tree_index: u64) -> LeafProof {
        let leaf = self.read_leaf(tree_index).unwrap();

        let hashes = self
            .tree
            .prove(self.version, &[leaf.key])
            .unwrap()
            .hashes
            .into_iter()
            .map(|h| fixed_bytes_to_bytes32(h.value));

        LeafProof::new(
            tree_index,
            FlatStorageLeaf {
                key: fixed_bytes_to_bytes32(leaf.key),
                value: fixed_bytes_to_bytes32(leaf.value),
                next: leaf.next_index,
            },
            hashes.collect::<Vec<_>>().try_into().unwrap(),
        )
    }

    fn prev_tree_index(&mut self, key: Bytes32) -> u64 {
        // TODO this will fail for existing nodes
        let res = &self
            .tree
            .db
            .indices(self.version, &[FixedBytes::from_slice(key.as_u8_ref())])
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

fn fixed_bytes_to_bytes32(x: B256) -> Bytes32 {
    let x: [u8; 32] = x.into();
    x.into()
}
