use alloy::primitives::{B256, keccak256};
use blake2::{Blake2s256, Digest};
use zksync_os_l1_sender::commitment::StoredBatchInfo;
use zksync_os_merkle_tree::{MerkleTree, PatchSet};

pub fn genesis_stored_batch_info() -> StoredBatchInfo {
    let number = 0u64;
    let timestamp = 0u64;

    let last_256_block_hashes_blake = {
        let mut blocks_hasher = Blake2s256::new();
        for _ in 0..255 {
            blocks_hasher.update([0u8; 32]);
        }
        blocks_hasher.update([0u8; 32]); // TODO: use genesis block hash

        blocks_hasher.finalize()
    };

    let empty_tree_hash = MerkleTree::<PatchSet>::empty_tree_hash();
    let leaf_count = 2u64;

    let mut hasher = Blake2s256::new();
    hasher.update(empty_tree_hash.as_slice());
    hasher.update(leaf_count.to_be_bytes());
    hasher.update(number.to_be_bytes());
    hasher.update(last_256_block_hashes_blake);
    hasher.update(timestamp.to_be_bytes());
    let state_commitment = B256::from_slice(&hasher.finalize());

    StoredBatchInfo {
        batch_number: 0,
        state_commitment,
        number_of_layer1_txs: 0,
        priority_operations_hash: keccak256([]),
        // `DEFAULT_L2_LOGS_TREE_ROOT_HASH` is explicitly set to zero in L1 contracts.
        // See `era-contracts/l1-contracts/contracts/common/Config.sol`.
        l2_to_l1_logs_root_hash: Default::default(),
        // TODO: Calculate from empty header
        commitment: "0x753b52ab98b0062963a4b2ea1c061c4ab522f53f50b8fefe0a52760cbcc9e183"
            .parse()
            .unwrap(),
    }
}
