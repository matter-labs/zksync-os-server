use alloy::primitives::B256;
use zksync_os_merkle_tree_api::{BatchTreeProof, TreeBatchOutput, TreeOperation};

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
    /// Batch proof of the storage update.
    pub proof: BatchTreeProof,
}

impl BlockMerkleTreeData {
    pub fn keys_and_ops(&self) -> impl Iterator<Item = (B256, TreeOperation)> {
        assert_eq!(self.proof.operations.len(), self.written_keys.len());
        assert_eq!(self.proof.read_operations.len(), self.read_keys.len());

        let written = self
            .written_keys
            .iter()
            .copied()
            .zip(self.proof.operations.iter().copied());
        let read = self
            .read_keys
            .iter()
            .copied()
            .zip(self.proof.read_operations.iter().copied());
        written.chain(read)
    }
}
