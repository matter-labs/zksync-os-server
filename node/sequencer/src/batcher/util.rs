use alloy::primitives::{B256, keccak256};
use blake2::{Blake2s256, Digest};
use ruint::aliases::{B160, U256};
use zk_ee::utils::Bytes32;
use zk_os_basic_system::system_implementation::system::BatchOutput;
use zksync_os_l1_sender::commitment::StoredBatchInfo;
use zksync_os_merkle_tree::{MerkleTreeForReading, RocksDBWrapper};
use zksync_os_storage::lazy::RepositoryManager;
use zksync_os_storage_api::ReadRepository;

pub async fn load_genesis_stored_batch_info(
    repository_manager: &RepositoryManager,
    tree: MerkleTreeForReading<RocksDBWrapper>,
    chain_id: u64,
) -> StoredBatchInfo {
    let genesis_block = repository_manager
        .get_block_by_number(0)
        .expect("Failed to read genesis block from repositories")
        .expect("Missing genesis block in repositories");
    let genesis_root_info = tree
        .get_at_block(0)
        .await
        .root_info()
        .expect("Failed to get genesis root info");

    let number = 0u64;
    let timestamp = 0u64;

    let last_256_block_hashes_blake = {
        let mut blocks_hasher = Blake2s256::new();
        for _ in 0..255 {
            blocks_hasher.update([0u8; 32]);
        }
        blocks_hasher.update(genesis_block.hash());

        blocks_hasher.finalize()
    };

    let mut hasher = Blake2s256::new();
    hasher.update(genesis_root_info.0.as_slice());
    hasher.update(genesis_root_info.1.to_be_bytes());
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
        commitment: genesis_batch_output(chain_id).hash().into(),
        last_block_timestamp: timestamp,
    }
}

fn genesis_batch_output(chain_id: u64) -> BatchOutput {
    BatchOutput {
        chain_id: U256::from_be_slice(&chain_id.to_be_bytes()),
        first_block_timestamp: 0,
        last_block_timestamp: 0,
        used_l2_da_validator_address: B160::ZERO,
        pubdata_commitment: Bytes32::ZERO,
        number_of_layer_1_txs: U256::ZERO,
        priority_operations_hash: Bytes32::from_array(keccak256([]).0),
        l2_logs_tree_root: Bytes32::ZERO,
        upgrade_tx_hash: Bytes32::ZERO,
    }
}
