use alloy::primitives::{Address, B256, U256, address, keccak256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use futures::TryFutureExt;
use std::collections::HashMap;
use std::ops;
use zksync_os_contract_interface::IMessageRoot::AppendedChainBatchRoot;
use zksync_os_contract_interface::{Bytes32PushTree, IMessageRoot};

const L2_MESSAGE_ROOT_ADDRESS: Address = address!("0x0000000000000000000000000000000000010005");

fn calculate_batch_tree_proof(
    mut tree: Bytes32PushTree,
    new_hashes: Vec<B256>,
    proof_for_idx: usize,
) -> Vec<B256> {
    assert!(proof_for_idx < new_hashes.len());

    // Add hashes up to the proof index.
    #[allow(clippy::needless_range_loop)]
    for i in 0..=proof_for_idx {
        push_to_tree(&mut tree, new_hashes[i]);
    }

    // Calculate the last level index as if all new hashes were added.
    let levels_with_all_leaves = {
        let last_index: u64 =
            tree._nextLeafIndex.to::<u64>() + new_hashes.len() as u64 - proof_for_idx as u64 - 2;
        match last_index {
            0 => 0,
            last_index => (last_index.ilog2() + 1) as usize,
        }
    };

    // Extend zeros if needed.
    let mut zeros = tree._zeros;
    while zeros.len() <= levels_with_all_leaves {
        let zero = *zeros.last().unwrap();
        let new_zero = keccak256([zero.0, zero.0].concat());
        zeros.push(new_zero);
    }

    let mut current_index = tree._nextLeafIndex.to::<u64>() - 1;
    let levels = zeros.len() - 1;

    let mut node_hash_calculator = NodeHashCalculator::new(
        new_hashes[(proof_for_idx + 1)..].to_vec(),
        current_index + 1,
        zeros,
    );
    let mut proof = Vec::new();

    // Build the proof from leaf to root.
    for i in 0..levels {
        let is_left = current_index.is_multiple_of(2);
        if is_left {
            proof.push(node_hash_calculator.node_hash(i, current_index + 1));
        } else {
            proof.push(tree._sides[i]);
        }
        current_index /= 2;
    }

    proof
}

/// Helper struct to calculate node hashes with caching.
#[derive(Debug)]
struct NodeHashCalculator {
    cache: HashMap<(usize, u64), B256>, // (level, index) -> hash
    leaves: Vec<B256>,
    first_leaf_index: u64,
    last_non_zero_indices: Vec<u64>, // per level
    zeros: Vec<B256>,
}

impl NodeHashCalculator {
    pub fn new(leaves: Vec<B256>, first_leaf_index: u64, zeros: Vec<B256>) -> Self {
        let levels = zeros.len();
        let mut last_non_zero_indices = vec![0; levels];

        let mut last_index = first_leaf_index + leaves.len() as u64 - 1;
        #[allow(clippy::needless_range_loop)]
        for level in 0..levels {
            last_non_zero_indices[level] = last_index;
            last_index /= 2;
        }

        Self {
            cache: HashMap::new(),
            leaves,
            first_leaf_index,
            last_non_zero_indices,
            zeros,
        }
    }

    pub fn node_hash(&mut self, level: usize, index: u64) -> B256 {
        if let Some(cached) = self.cache.get(&(level, index)) {
            return *cached;
        }

        let hash = self.node_hash_internal(level, index);

        self.cache.insert((level, index), hash);
        hash
    }

    fn node_hash_internal(&mut self, level: usize, index: u64) -> B256 {
        assert!(level < self.zeros.len());

        // If the index is beyond the last non-zero index at this level, return zero.
        if index > self.last_non_zero_indices[level] {
            return self.zeros[level];
        }

        // If we are at leaf level, return the corresponding leaf.
        if level == 0 {
            let range = self.first_leaf_index..(self.first_leaf_index + self.leaves.len() as u64);
            assert!(range.contains(&index));

            return self.leaves[(index - self.first_leaf_index) as usize];
        }

        // Otherwise, compute the hash from children.
        let left_child_hash = self.node_hash(level - 1, index * 2);
        let right_child_hash = self.node_hash(level - 1, index * 2 + 1);

        keccak256([left_child_hash.0, right_child_hash.0].concat())
    }
}

fn push_to_tree(tree: &mut Bytes32PushTree, leaf: B256) {
    let mut levels = tree._zeros.len() - 1;
    let index = tree._nextLeafIndex;
    tree._nextLeafIndex += U256::ONE;

    if index == U256::from(2u32).pow(U256::from(levels)) {
        let zero = tree._zeros[levels];
        let new_zero = keccak256([zero.0, zero.0].concat());
        tree._zeros.push(new_zero);
        tree._sides.push(B256::ZERO);
        levels += 1;
    }

    // Rebuild branch from leaf to root
    let mut current_index = index;
    let mut current_level_hash = leaf;
    let mut updated_sides = false;
    for i in 0..levels {
        // Reaching the parent node, is currentLevelHash the left child?
        let is_left = current_index % U256::from(2u32) == U256::ZERO;

        // If so, next time we will come from the right, so we need to save it
        if is_left && !updated_sides {
            tree._sides[i] = current_level_hash;
            updated_sides = true;
        }

        // Compute the current node hash by using the hash function
        // with either its sibling (side) or the zero value for that level.
        current_level_hash = if is_left {
            keccak256([current_level_hash.0, tree._zeros[i].0].concat())
        } else {
            keccak256([tree._sides[i].0, current_level_hash.0].concat())
        };

        // Update node index
        current_index /= U256::from(2u32);
    }

    tree._sides[levels] = current_level_hash;
}

#[derive(Debug, Clone)]
pub struct ChainAggProof {
    pub chain_id_leaf_proof: Vec<B256>,
    pub chain_id_leaf_proof_mask: U256,
}

pub async fn get_chain_log_proof(
    l2_chain_id: u64,
    gw_block_number: u64,
    gw_provider: &DynProvider,
) -> anyhow::Result<ChainAggProof> {
    let message_root = IMessageRoot::new(L2_MESSAGE_ROOT_ADDRESS, gw_provider.clone());
    let merkle_path_builder = message_root
        .getMerklePathForChain(U256::from(l2_chain_id))
        .block(gw_block_number.into());
    let merkle_path_fut = merkle_path_builder
        .call()
        .into_future()
        .map_err(|e| anyhow::Error::from(e).context("getMerklePathForChain"));
    let chain_index_builder = message_root.chainIndex(U256::from(l2_chain_id));
    let chain_index_fut = chain_index_builder
        .call()
        .into_future()
        .map_err(|e| anyhow::Error::from(e).context("chainIndex"));
    let (merkle_path, chain_index) =
        futures::future::try_join(merkle_path_fut, chain_index_fut).await?;
    Ok(ChainAggProof {
        chain_id_leaf_proof: merkle_path,
        chain_id_leaf_proof_mask: chain_index,
    })
}

pub fn chain_proof_vector(
    batch_or_block_number: u64,
    chain_agg_proof: ChainAggProof,
    sl_chain_id: u64,
) -> Vec<B256> {
    const LOG_PROOF_SUPPORTED_METADATA_VERSION: u8 = 1;

    let sl_encoded_data = (U256::from(batch_or_block_number) << U256::from(128u32))
        + chain_agg_proof.chain_id_leaf_proof_mask;

    let mut metadata = [0u8; 32];
    metadata[0] = LOG_PROOF_SUPPORTED_METADATA_VERSION;
    metadata[1] = chain_agg_proof.chain_id_leaf_proof.len() as u8;

    // Chain proofs are always final nodes in the proofs.
    metadata[3] = 1;

    let mut chain_proof_vector = vec![
        B256::from(sl_encoded_data.to_be_bytes()),
        B256::from(U256::from(sl_chain_id).to_be_bytes()),
        metadata.into(),
    ];
    chain_proof_vector.extend(chain_agg_proof.chain_id_leaf_proof);

    chain_proof_vector
}

pub async fn batch_tree_proof(
    gw_block_range: ops::RangeInclusive<u64>,
    l2_chain_id: u64,
    batch_number: u64,
    gw_provider: &DynProvider,
) -> anyhow::Result<(Vec<B256>, u8)> {
    assert!(*gw_block_range.start() > 0);

    let message_root = IMessageRoot::new(L2_MESSAGE_ROOT_ADDRESS, gw_provider.clone());
    let tree_call_builder = message_root
        .getChainTree(U256::from(l2_chain_id))
        .block((gw_block_range.start() - 1).into());
    let tree_future = tree_call_builder
        .call()
        .into_future()
        .map_err(|e| anyhow::Error::from(e).context("getChainTree"));

    let filter = Filter::new()
        .from_block(*gw_block_range.start())
        .to_block(*gw_block_range.end())
        .event_signature(AppendedChainBatchRoot::SIGNATURE_HASH)
        .topic1(U256::from(l2_chain_id))
        .address(L2_MESSAGE_ROOT_ADDRESS);
    let logs_future = gw_provider
        .get_logs(&filter)
        .map_err(|e| anyhow::Error::from(e).context("get_logs for AppendedChainBatchRoot"));

    let (tree, logs) = futures::future::try_join(tree_future, logs_future).await?;

    let batch_leaf_padding: B256 = keccak256(b"zkSync:BatchLeaf");
    let batch_idx = logs
        .iter()
        .position(|log| {
            let log_batch_number = log.inner.topics()[2];
            log_batch_number == U256::from(batch_number).to_be_bytes()
        })
        .ok_or_else(|| anyhow::anyhow!("Batch number {} not found in logs", batch_number))?;
    let absolute_batch_idx = tree._nextLeafIndex.to::<usize>() + batch_idx;
    let new_hashes: Vec<B256> = logs
        .into_iter()
        .map(|log| {
            let batch_root = B256::from_slice(&log.inner.data.data);
            let batch_number = log.inner.topics()[2];
            let preimage = [batch_leaf_padding.0, batch_root.0, batch_number.0].concat();
            keccak256(preimage)
        })
        .collect();

    let batch_proof = calculate_batch_tree_proof(tree, new_hashes, batch_idx);
    let batch_proof_len = batch_proof.len() as u8;

    let mut proof = vec![B256::from(U256::from(absolute_batch_idx).to_be_bytes())];
    proof.extend(batch_proof);

    Ok((proof, batch_proof_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::b256;

    #[test]
    fn test_calculate_batch_tree_proof() {
        const ZERO: B256 =
            b256!("0x46700b4d40ac5c35af2c22dda2787a91eb567b06c924a8fb8ae9a05b20c08c21");

        let empty_tree = Bytes32PushTree {
            _nextLeafIndex: U256::ZERO,
            _zeros: vec![ZERO],
            _sides: vec![B256::ZERO],
        };
        let mut hashes = Vec::new();
        for i in 0..20 {
            hashes.push(keccak256([i as u8; 32]));
        }

        for prefilled in 0..hashes.len() {
            let mut tree_with_prefilled = empty_tree.clone();
            for h in &hashes[0..prefilled] {
                push_to_tree(&mut tree_with_prefilled, *h);
            }

            for new_len in 1..(hashes.len() - prefilled) {
                let new_hashes = hashes[prefilled..(prefilled + new_len)].to_vec();
                let mut tree = tree_with_prefilled.clone();
                for h in &new_hashes {
                    push_to_tree(&mut tree, *h);
                }
                for i in 0..new_hashes.len() {
                    let proof = calculate_batch_tree_proof(
                        tree_with_prefilled.clone(),
                        new_hashes.clone(),
                        i,
                    );

                    // Verify proof
                    let mut current_hash = new_hashes[i];
                    let mut current_index: u64 =
                        tree_with_prefilled._nextLeafIndex.to::<u64>() + i as u64;
                    for sibling_hash in proof.iter() {
                        let is_left = current_index.is_multiple_of(2);
                        current_hash = if is_left {
                            keccak256([current_hash.0, sibling_hash.0].concat())
                        } else {
                            keccak256([sibling_hash.0, current_hash.0].concat())
                        };
                        current_index /= 2;
                    }

                    assert_eq!(current_hash, tree._sides[tree._zeros.len() - 1]);
                }
            }
        }
    }
}
