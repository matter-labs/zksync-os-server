use zk_ee::utils::Bytes32;
use zksync_os_merkle_tree_api::{BatchTreeProof, TreeBatchOutput, TreeOperation};

#[derive(Debug, Clone)]
pub struct BlockMerkleTreeData {
    pub output: TreeBatchOutput,
    pub written_keys: Vec<Bytes32>,
    pub read_keys: Vec<Bytes32>,
    pub proof: BatchTreeProof,
}

impl BlockMerkleTreeData {
    pub fn keys_and_ops(&self) -> impl Iterator<Item = (Bytes32, TreeOperation)> {
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
