use crate::{
    leaf_nibbles,
    types::{KeyLookup, Leaf, Node, NodeKey},
    Database, HashTree, MerkleTree, TreeParams,
};
use alloy::primitives::{FixedBytes, B256};
use zk_ee::utils::Bytes32;
use zk_os_basic_system::system_implementation::flat_storage_model::FlatStorageLeaf;
use zk_os_forward_system::run::{LeafProof, ReadStorage, ReadStorageTree};

pub struct MerkleTreeVersion<DB: Database, P: TreeParams> {
    pub tree: MerkleTree<DB, P>,
    pub version: u64,
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

        let empty_leaf_hash = self.tree.hasher.hash_leaf(&Leaf::default());
        let empty_hashes: Vec<_> = core::iter::successors(Some(empty_leaf_hash), |previous| {
            Some(self.tree.hasher.hash_branch(&previous, &previous))
        })
        .take(P::TREE_DEPTH.into())
        .collect();

        let node_keys: Vec<_> = (1..leaf_nibbles::<P>())
            .rev()
            .map(|nibble_count| NodeKey {
                version: self.version,
                nibble_count,
                index_on_level: tree_index
                    >> ((leaf_nibbles::<P>() - nibble_count) * P::INTERNAL_NODE_DEPTH),
            })
            .collect();
        let nodes = self.tree.db.try_nodes(&node_keys).unwrap();

        let mut sibling_hashes = Box::new([Bytes32::zero(); 64]);
        let mut i = 0;
        let mut position = tree_index;
        for node in nodes {
            let node = match node {
                Node::Internal(internal_node) => internal_node,
                Node::Leaf(_) => unreachable!(),
            };
            let hashes = node.internal_hashes::<P>(&self.tree.hasher, i).0;

            let position_in_node = position as u8 & ((1 << P::INTERNAL_NODE_DEPTH) - 1);
            position >>= P::INTERNAL_NODE_DEPTH;

            for depth in (0..P::INTERNAL_NODE_DEPTH).rev() {
                let index_on_level =
                    (position_in_node >> (P::INTERNAL_NODE_DEPTH - depth - 1)) as usize;
                let index_on_level = index_on_level ^ 1;
                sibling_hashes[i as usize] =
                    fixed_bytes_to_bytes32(if depth == P::INTERNAL_NODE_DEPTH - 1 {
                        node.children
                            .get(index_on_level)
                            .map(|x| x.hash)
                            .unwrap_or(empty_hashes[i as usize])
                    } else {
                        let needed_for_this_and_lower_levels = (2 << (depth + 1)) - 2;
                        let needed_for_all = (2 << (P::INTERNAL_NODE_DEPTH - 1)) - 2;
                        let skip = needed_for_all - needed_for_this_and_lower_levels;
                        let hash = hashes[skip + index_on_level];
                        // TODO: this is wrong but not sure how to properly detect empty
                        if hash == B256::default() {
                            empty_hashes[i as usize]
                        } else {
                            hash
                        }
                    });
                i += 1;
            }
        }

        let last = P::TREE_DEPTH as usize - 1;
        let root = self
            .tree
            .db
            .try_root(self.version)
            .unwrap()
            .unwrap()
            .root_node;
        sibling_hashes[last] = fixed_bytes_to_bytes32(
            root.children
                .get(position as usize ^ 1)
                .map(|x| x.hash)
                .unwrap_or(empty_hashes[last]),
        );

        LeafProof::new(
            tree_index,
            FlatStorageLeaf {
                key: fixed_bytes_to_bytes32(leaf.key),
                value: fixed_bytes_to_bytes32(leaf.value),
                next: leaf.next_index,
            },
            sibling_hashes,
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
