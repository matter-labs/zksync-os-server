use alloy::primitives::{B256, U256, keccak256};
use blake2::{Blake2s256, Digest};
use zksync_os_contract_interface::models::StoredBatchInfo;
use zksync_os_genesis::GenesisState;

pub async fn load_genesis_stored_batch_info(
    genesis_state: &GenesisState,
    genesis_root_hash: B256,
    genesis_root_leaves: u64,
) -> anyhow::Result<StoredBatchInfo> {
    let number = 0u64;
    let timestamp = 0u64;

    let last_256_block_hashes_blake = {
        let mut blocks_hasher = Blake2s256::new();
        for _ in 0..255 {
            blocks_hasher.update([0u8; 32]);
        }
        blocks_hasher.update(genesis_state.header.hash());

        blocks_hasher.finalize()
    };

    let mut hasher = Blake2s256::new();
    hasher.update(genesis_root_hash.as_slice());
    hasher.update(genesis_root_leaves.to_be_bytes());
    hasher.update(number.to_be_bytes());
    hasher.update(last_256_block_hashes_blake);
    hasher.update(timestamp.to_be_bytes());
    let state_commitment = B256::from_slice(&hasher.finalize());

    anyhow::ensure!(
        genesis_state.expected_genesis_root == state_commitment,
        "Genesis state commitment mismatch, expected from genesis.json {:?}, calculated {state_commitment:?}",
        genesis_state.expected_genesis_root
    );

    Ok(StoredBatchInfo {
        batch_number: 0,
        state_commitment,
        number_of_layer1_txs: 0,
        priority_operations_hash: keccak256([]),
        dependency_roots_rolling_hash: B256::ZERO,
        // `DEFAULT_L2_LOGS_TREE_ROOT_HASH` is explicitly set to zero in L1 contracts.
        // See `era-contracts/l1-contracts/contracts/common/Config.sol`.
        l2_to_l1_logs_root_hash: B256::ZERO,
        commitment: B256::from(U256::ONE.to_be_bytes()),
        last_block_timestamp: timestamp,
    })
}
