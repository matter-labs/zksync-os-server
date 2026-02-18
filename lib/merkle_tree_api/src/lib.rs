//! ZK OS Merkle tree API.

pub use zksync_os_crypto::hasher::{Hasher, blake2::Blake2Hasher};

pub use crate::{
    hasher::HashTree,
    proofs::{BatchTreeProof, IntermediateHash, TreeOperation},
    types::{Leaf, MAX_TREE_DEPTH, TreeBatchOutput, TreeEntry},
};

mod hasher;
mod proofs;
mod types;
