use std::iter;
use zksync_os_crypto::hasher::Hasher;
use zksync_os_crypto::hasher::blake2::Blake2Hasher;

pub(crate) use self::nodes::InternalHashes;
pub use self::proofs::{BatchTreeProof, IntermediateHash, TreeOperation};
use crate::types::{Leaf, MAX_TREE_DEPTH};
use alloy::primitives::B256;
use once_cell::sync::Lazy;

mod nodes;
mod proofs;

/// Tree hashing functionality.
pub trait HashTree: Send + Sync {
    /// Returns the unique name of the hasher. This is used in Merkle tree tags to ensure
    /// that the tree remains consistent.
    fn name(&self) -> &'static str;

    /// Hashes a leaf node.
    fn hash_leaf(&self, leaf: &Leaf) -> B256;
    /// Compresses hashes in an intermediate node of a binary Merkle tree.
    fn hash_branch(&self, lhs: &B256, rhs: &B256) -> B256;

    /// Returns the hash of an empty subtree with the given depth. `depth == 0` corresponds to leaves. Implementations
    /// are encouraged to cache the returned values.
    ///
    /// Guaranteed to never be called with `depth > 64` (i.e., exceeding the depth of the entire tree).
    fn empty_subtree_hash(&self, depth: u8) -> B256;
}

impl<H: HashTree + ?Sized> HashTree for &H {
    fn name(&self) -> &'static str {
        (**self).name()
    }

    fn hash_leaf(&self, leaf: &Leaf) -> B256 {
        (**self).hash_leaf(leaf)
    }

    fn hash_branch(&self, lhs: &B256, rhs: &B256) -> B256 {
        (**self).hash_branch(lhs, rhs)
    }

    fn empty_subtree_hash(&self, depth: u8) -> B256 {
        (**self).empty_subtree_hash(depth)
    }
}

/// No-op implementation.
impl HashTree for () {
    fn name(&self) -> &'static str {
        "no-op"
    }

    fn hash_leaf(&self, _leaf: &Leaf) -> B256 {
        B256::ZERO
    }

    fn hash_branch(&self, _lhs: &B256, _rhs: &B256) -> B256 {
        B256::ZERO
    }

    fn empty_subtree_hash(&self, _depth: u8) -> B256 {
        B256::ZERO
    }
}

impl HashTree for Blake2Hasher {
    fn name(&self) -> &'static str {
        "Blake2s256"
    }

    fn hash_leaf(&self, leaf: &Leaf) -> B256 {
        let mut hashed_bytes = [0; 2 * 32 + 8];
        hashed_bytes[..32].copy_from_slice(leaf.key.as_slice());
        hashed_bytes[32..64].copy_from_slice(leaf.value.as_slice());
        hashed_bytes[64..].copy_from_slice(&leaf.next_index.to_le_bytes());
        self.hash_bytes(&hashed_bytes)
    }

    fn hash_branch(&self, lhs: &B256, rhs: &B256) -> B256 {
        self.compress(lhs, rhs)
    }

    fn empty_subtree_hash(&self, depth: u8) -> B256 {
        static EMPTY_TREE_HASHES: Lazy<Vec<B256>> = Lazy::new(compute_empty_tree_hashes);
        EMPTY_TREE_HASHES[usize::from(depth)]
    }
}

fn compute_empty_tree_hashes() -> Vec<B256> {
    let empty_leaf_hash = Blake2Hasher.hash_leaf(&Leaf::default());
    iter::successors(Some(empty_leaf_hash), |hash| {
        Some(Blake2Hasher.hash_branch(hash, hash))
    })
    .take(usize::from(MAX_TREE_DEPTH) + 1)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_leaves_is_correct() {
        let expected_empty_leaf_hash: B256 =
            "0xe3cdc93b3c2beb30f6a7c7cc45a32da012df9ae1be880e2c074885cb3f4e1e53"
                .parse()
                .unwrap();
        assert_eq!(Blake2Hasher.empty_subtree_hash(0), expected_empty_leaf_hash);

        let expected_level1_empty_hash: B256 =
            "0xc45bfaf4bb5d0fee27d3178b8475155a07a1fa8ada9a15133a9016f7d0435f0f"
                .parse()
                .unwrap();
        assert_eq!(
            Blake2Hasher.empty_subtree_hash(1),
            expected_level1_empty_hash
        );

        let expected_level63_empty_hash: B256 =
            "0xb720fe53e6bd4e997d967b8649e10036802a4fd3aca6d7dcc43ed9671f41cb31"
                .parse()
                .unwrap();
        assert_eq!(
            Blake2Hasher.empty_subtree_hash(63),
            expected_level63_empty_hash
        );

        let expected_min_guard_hash: B256 =
            "0x9903897e51baa96a5ea51b4c194d3e0c6bcf20947cce9fd646dfb4bf754c8d28"
                .parse()
                .unwrap();
        assert_eq!(
            Blake2Hasher.hash_leaf(&Leaf::MIN_GUARD),
            expected_min_guard_hash
        );

        let expected_max_guard_hash: B256 =
            "0xb35299e7564e05e335094c02064bccf83d58745b417874b1fee3f523ec2007a9"
                .parse()
                .unwrap();
        assert_eq!(
            Blake2Hasher.hash_leaf(&Leaf::MAX_GUARD),
            expected_max_guard_hash
        );
    }
}
