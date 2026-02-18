//! Basic types used by the Merkle tree.

use alloy::primitives::B256;

/// Maximum supported tree depth (to fit indexes into `u64`).
pub const MAX_TREE_DEPTH: u8 = 64;

/// Entry in a Merkle tree associated with a key. Provided as an input for Merkle tree operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TreeEntry {
    /// Tree key.
    pub key: B256,
    /// Value associated with the key.
    pub value: B256,
}

impl TreeEntry {
    pub const MIN_GUARD: Self = Self {
        key: B256::ZERO,
        value: B256::ZERO,
    };

    pub const MAX_GUARD: Self = Self {
        key: B256::repeat_byte(0xff),
        value: B256::ZERO,
    };
}

/// Tree leaf.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Leaf {
    pub key: B256,
    pub value: B256,
    /// 0-based index of a leaf with the lexicographically next key.
    pub next_index: u64,
}

impl Leaf {
    /// Minimum guard leaf inserted at the tree at its initialization.
    pub const MIN_GUARD: Self = Self {
        key: B256::ZERO,
        value: B256::ZERO,
        next_index: 1,
    };

    /// Maximum guard leaf inserted at the tree at its initialization.
    pub const MAX_GUARD: Self = Self {
        key: B256::repeat_byte(0xff),
        value: B256::ZERO,
        // Circular pointer to self; never updated.
        next_index: 1,
    };
}

/// Output of updating / inserting data in a Merkle tree.
#[derive(Debug, Clone, Copy)]
pub struct TreeBatchOutput {
    /// New root hash of the tree.
    pub root_hash: B256,
    /// New leaf count (including 2 guard entries).
    pub leaf_count: u64,
}
