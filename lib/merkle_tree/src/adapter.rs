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
        let mut sibling_hashes = Box::new([Bytes32::zero(); 64]);

        let mut current_node = self
            .tree
            .db()
            .try_root(self.version)
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
                    sibling_hashes[i] = fixed_bytes_to_bytes32(hashes[skip + (index ^ 1)]);
                }
            }

            i -= 1;
            sibling_hashes[i] = fixed_bytes_to_bytes32(
                current_node
                    .children
                    .get(child_index ^ 1)
                    .map(|x| x.hash)
                    .unwrap_or(self.tree.hasher.empty_subtree_hash(i as u8)),
            );

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
            sibling_hashes[i] =
                fixed_bytes_to_bytes32(self.tree.hasher.empty_subtree_hash(i as u8));
        }

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
