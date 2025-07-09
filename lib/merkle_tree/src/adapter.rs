use crate::{
    errors::DeserializeErrorKind,
    leaf_nibbles,
    types::{KeyLookup, Leaf, Node, NodeKey},
    Database, DeserializeError, HashTree, MerkleTree, TreeParams,
};
use alloy::primitives::{FixedBytes, B256};
use zk_ee::utils::Bytes32;
use zk_os_basic_system::system_implementation::flat_storage_model::FlatStorageLeaf;
use zk_os_forward_system::run::{ReadStorage, ReadStorageTree, SimpleReadStorageTree};

pub struct MerkleTreeVersion<DB: Database, P: TreeParams> {
    pub tree: MerkleTree<DB, P>,
    pub version: u64,
}

impl<DB: Database, P: TreeParams> MerkleTreeVersion<DB, P> {
    fn read_leaf(&mut self, index: u64) -> Option<Leaf> {
        self.try_node(NodeKey {
            version: self.version,
            nibble_count: leaf_nibbles::<P>(),
            index_on_level: index,
        })
        .map(|n| match n {
            Node::Internal(_) => unreachable!(),
            Node::Leaf(leaf) => leaf,
        })
    }

    fn try_node(&mut self, mut key: NodeKey) -> Option<Node> {
        loop {
            let node = self.tree.db.try_nodes(&[key.clone()]);
            match node {
                Ok(vec) => return Some(vec[0].clone()),
                Err(DeserializeError {
                    kind: DeserializeErrorKind::MissingNode,
                    ..
                }) => {}
                Err(e) => panic!("{}", e),
            }

            if key.version == 0 {
                return None;
            }
            key.version -= 1;
        }
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

impl<DB: Database + 'static, P: TreeParams + 'static> SimpleReadStorageTree
    for MerkleTreeVersion<DB, P>
{
    fn simple_tree_index(&mut self, key: Bytes32) -> Option<u64> {
        self.tree
            .db()
            .indices(self.version, &[FixedBytes::from_slice(key.as_u8_ref())])
            .ok()
            .and_then(|v| match v[0] {
                KeyLookup::Existing(x) => Some(x),
                KeyLookup::Missing { .. } => None,
            })
    }

    fn simple_merkle_proof(
        &mut self,
        tree_index: u64,
    ) -> (u64, FlatStorageLeaf<64>, Box<[Bytes32; 64]>) {
        let leaf = self.read_leaf(tree_index).unwrap_or_default();

        let empty_leaf_hash = self.tree.hasher.hash_leaf(&Leaf::default());
        let empty_hashes: Vec<_> = core::iter::successors(Some(empty_leaf_hash), |previous| {
            Some(self.tree.hasher.hash_branch(previous, previous))
        })
        .take(P::TREE_DEPTH.into())
        .collect();

        let mut sibling_hashes = Box::new([Bytes32::zero(); 64]);
        let mut i = 0;
        let mut position = tree_index;
        for nibble_count in (1..leaf_nibbles::<P>()).rev() {
            let position_in_node = position as u8 & ((1 << P::INTERNAL_NODE_DEPTH) - 1);
            position >>= P::INTERNAL_NODE_DEPTH;

            let result = self.try_node(NodeKey {
                version: self.version,
                nibble_count,
                index_on_level: tree_index
                    >> ((leaf_nibbles::<P>() - nibble_count) * P::INTERNAL_NODE_DEPTH),
            });
            let node = match result {
                Some(n) => match n {
                    Node::Internal(internal_node) => internal_node,
                    Node::Leaf(_) => unreachable!(),
                },
                None => {
                    for _ in 0..P::INTERNAL_NODE_DEPTH {
                        sibling_hashes[i] = fixed_bytes_to_bytes32(empty_hashes[i]);
                        i += 1;
                    }
                    continue;
                }
            };
            let hashes = node.internal_hashes::<P>(&self.tree.hasher, i as u8).0;

            for depth in (0..P::INTERNAL_NODE_DEPTH).rev() {
                let index_on_level =
                    (position_in_node >> (P::INTERNAL_NODE_DEPTH - depth - 1)) as usize;
                let index_on_level = index_on_level ^ 1;
                sibling_hashes[i] =
                    fixed_bytes_to_bytes32(if depth == P::INTERNAL_NODE_DEPTH - 1 {
                        node.children
                            .get(index_on_level)
                            .map(|x| x.hash)
                            .unwrap_or(empty_hashes[i])
                    } else {
                        let needed_for_this_and_lower_levels = (2 << (depth + 1)) - 2;
                        let needed_for_all = (2 << (P::INTERNAL_NODE_DEPTH - 1)) - 2;
                        let skip = needed_for_all - needed_for_this_and_lower_levels;
                        let hash = hashes[skip + index_on_level];
                        // TODO: this is wrong but not sure how to properly detect empty
                        if hash == B256::default() {
                            empty_hashes[i]
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
            .db()
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

        (
            tree_index,
            FlatStorageLeaf {
                key: fixed_bytes_to_bytes32(leaf.key),
                value: fixed_bytes_to_bytes32(leaf.value),
                next: leaf.next_index,
            },
            sibling_hashes,
        )
    }

    fn simple_prev_tree_index(&mut self, key: Bytes32) -> u64 {
        // TODO this will fail for existing nodes
        let res = &self
            .tree
            .db()
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
