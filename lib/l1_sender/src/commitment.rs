use alloy::primitives::{Address, B256, keccak256};
use blake2::{Blake2s256, Digest};
use zksync_os_contract_interface::models::CommitBatchInfo;
use zksync_os_interface::types::{BlockContext, BlockOutput};
use zksync_os_mini_merkle_tree::MiniMerkleTree;
use zksync_os_types::{L2_TO_L1_TREE_SIZE, L2ToL1Log, ZkEnvelope, ZkTransaction};

const PUBDATA_SOURCE_CALLDATA: u8 = 0;

pub fn build_commit_batch_info(
    blocks: Vec<(
        &BlockOutput,
        &BlockContext,
        &[ZkTransaction],
        &zksync_os_merkle_tree::TreeBatchOutput,
    )>,
    chain_id: u64,
    chain_address: Address,
    batch_number: u64,
) -> CommitBatchInfo {
    let mut priority_operations_hash = keccak256([]);
    let mut number_of_layer1_txs = 0;
    let mut total_pubdata = vec![];
    let mut encoded_l2_l1_logs = vec![];

    let (first_block_output, _, _, _) = *blocks.first().unwrap();
    let (last_block_output, last_block_context, _, last_block_tree) = *blocks.last().unwrap();

    let mut upgrade_tx_hash = None;
    for (block_output, _, transactions, _) in blocks {
        total_pubdata.extend(block_output.pubdata.clone());

        for tx in transactions {
            match tx.envelope() {
                ZkEnvelope::L1(l1_tx) => {
                    let onchain_data_hash = l1_tx.hash();
                    priority_operations_hash =
                        keccak256([priority_operations_hash.0, onchain_data_hash.0].concat());
                    number_of_layer1_txs += 1;
                }
                ZkEnvelope::L2(_) => {}
                ZkEnvelope::Upgrade(_) => {
                    assert!(
                        upgrade_tx_hash.is_none(),
                        "more than one upgrade tx in a batch: first {upgrade_tx_hash:?}, second {}",
                        tx.hash()
                    );
                    upgrade_tx_hash = Some(*tx.hash());
                }
            }
        }

        for tx_output in block_output.tx_results.clone().into_iter().flatten() {
            encoded_l2_l1_logs.extend(tx_output.l2_to_l1_logs.into_iter().map(
                |log_with_preimage| {
                    let log = L2ToL1Log {
                        l2_shard_id: log_with_preimage.log.l2_shard_id,
                        is_service: log_with_preimage.log.is_service,
                        tx_number_in_block: log_with_preimage.log.tx_number_in_block,
                        sender: log_with_preimage.log.sender,
                        key: log_with_preimage.log.key,
                        value: log_with_preimage.log.value,
                    };
                    log.encode()
                },
            ));
        }
    }

    let last_256_block_hashes_blake = {
        let mut blocks_hasher = Blake2s256::new();
        for block_hash in &last_block_context.block_hashes.0[1..] {
            blocks_hasher.update(block_hash.to_be_bytes::<32>());
        }
        blocks_hasher.update(last_block_output.header.hash());

        blocks_hasher.finalize()
    };

    /* ---------- operator DA input ---------- */
    let mut operator_da_input: Vec<u8> = vec![];

    // reference for this header is taken from zk_ee: https://github.com/matter-labs/zk_ee/blob/ad-aggregation-program/aggregator/src/aggregation/da_commitment.rs#L27
    // consider reusing that code instead:
    //
    // hasher.update([0u8; 32]); // we don't have to validate state diffs hash
    // hasher.update(Keccak256::digest(&pubdata)); // full pubdata keccak
    // hasher.update([1u8]); // with calldata we should provide 1 blob
    // hasher.update([0u8; 32]); // its hash will be ignored on the settlement layer
    // Ok(hasher.finalize().into())

    operator_da_input.extend(B256::ZERO.as_slice());
    operator_da_input.extend(keccak256(&total_pubdata));
    operator_da_input.push(1);
    operator_da_input.extend(B256::ZERO.as_slice());

    //     bytes32 daCommitment; - we compute hash of the first part of the operator_da_input (see above)
    let operator_da_input_header_hash = keccak256(&operator_da_input);

    operator_da_input.extend([PUBDATA_SOURCE_CALLDATA]);
    operator_da_input.extend(&total_pubdata);
    // blob_commitment should be set to zero in ZK OS
    operator_da_input.extend(B256::ZERO.as_slice());

    /* ---------- new state commitment ---------- */
    let mut hasher = Blake2s256::new();
    hasher.update(last_block_tree.root_hash.as_slice());
    hasher.update(last_block_tree.leaf_count.to_be_bytes());
    hasher.update(last_block_output.header.number.to_be_bytes());
    hasher.update(last_256_block_hashes_blake);
    hasher.update(last_block_output.header.timestamp.to_be_bytes());
    let new_state_commitment = B256::from_slice(&hasher.finalize());

    /* ---------- root hash of l2->l1 logs ---------- */
    let l2_l1_local_root = MiniMerkleTree::new(
        encoded_l2_l1_logs.clone().into_iter(),
        Some(L2_TO_L1_TREE_SIZE),
    )
    .merkle_root();
    // The result should be Keccak(l2_l1_local_root, aggreagation_root) - we don't compute aggregation root yet
    let l2_to_l1_logs_root_hash = keccak256([l2_l1_local_root.0, [0u8; 32]].concat());

    CommitBatchInfo {
        batch_number,
        new_state_commitment,
        number_of_layer1_txs,
        priority_operations_hash,
        dependency_roots_rolling_hash: B256::ZERO,
        l2_to_l1_logs_root_hash,
        // TODO: Update once enforced, not sure where to source it from yet
        l2_da_validator: Default::default(),
        da_commitment: operator_da_input_header_hash,
        first_block_timestamp: first_block_output.header.timestamp,
        last_block_timestamp: last_block_output.header.timestamp,
        chain_id,
        chain_address,
        operator_da_input,
        upgrade_tx_hash,
    }
}
