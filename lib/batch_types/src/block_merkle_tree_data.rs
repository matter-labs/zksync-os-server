use alloy::primitives::B256;
use zksync_os_merkle_tree_api::{BatchTreeProof, TreeBatchOutput};

/// Data necessary for the Merkle tree to produce a self-contained proof of batch storage update
/// as a result of block execution. This proof is then used by the proof input generator.
#[derive(Debug)]
pub struct BlockMerkleTreeData {
    /// Key tree parameters (root hash + number of leaves) **before** block execution.
    pub input: TreeBatchOutput,
    /// Key tree parameters (root hash + number of leaves) **after** block execution.
    pub output: TreeBatchOutput,
    /// Unique storage slots written during block execution. The order matches to the order of write ops
    /// in [`Self.proof`].
    pub written_keys: Vec<B256>,
    /// Unique storage slots read, but not written to, during block execution. The order matches to the order of read ops
    /// in [`Self.proof`].
    pub read_keys: Vec<B256>,
    /// Batch proof of the storage update. Only proves a proof of `written_keys`. The proof for `read_keys`
    /// is obtained asynchronously.
    pub proof: BatchTreeProof,
}
