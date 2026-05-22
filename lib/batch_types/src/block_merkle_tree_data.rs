use zksync_os_merkle_tree::MerkleTreeVersion;

#[derive(Clone)]
pub struct BlockMerkleTreeData {
    pub block_start: MerkleTreeVersion,
    pub block_end: MerkleTreeVersion,
}
