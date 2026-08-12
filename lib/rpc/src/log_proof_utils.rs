//! Builds the settlement-layer extension of an L2-to-L1 log proof.
//!
//! The base proof ends at the source chain's batch root. A `MessageRoot` proof must continue
//! through two append-only trees: the batch root into that chain's tree, then the chain root into
//! L1's shared interop tree. This module reconstructs those two proof segments from historical
//! `MessageRoot` state and events.

use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Context;
use futures::TryFutureExt;
use std::collections::HashMap;
use zksync_os_contract_interface::IMessageRoot::AppendedChainBatchRoot;
use zksync_os_contract_interface::{Bytes32PushTree, IMessageRoot};

const LOG_PROOF_SUPPORTED_METADATA_VERSION: u8 = 1;

/// Reconstructs the sibling path for one newly appended batch leaf.
///
/// `tree` is the state before the relevant block range and `new_hashes` are the leaves appended in
/// that range. Leaves through `proof_for_idx` update the stored left sides; later leaves are used to
/// calculate right-side siblings without mutating the historical tree snapshot.
fn calculate_batch_tree_proof(
    mut tree: Bytes32PushTree,
    new_hashes: Vec<B256>,
    proof_for_idx: usize,
) -> Vec<B256> {
    assert!(proof_for_idx < new_hashes.len());

    for hash in new_hashes.iter().take(proof_for_idx + 1) {
        push_to_tree(&mut tree, *hash);
    }

    // The returned proof targets the tree after the whole range, even though the compact tree only
    // needs to be mutated through the proven leaf.
    let levels_with_all_leaves = {
        let last_index: u64 =
            tree._nextLeafIndex.to::<u64>() + new_hashes.len() as u64 - proof_for_idx as u64 - 2;
        match last_index {
            0 => 0,
            last_index => (last_index.ilog2() + 1) as usize,
        }
    };

    // Grow zero subtrees to that final height before calculating right-side siblings.
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

/// Calculates right-side nodes contributed by leaves appended after the proven leaf.
///
/// A proof can ask for the same subtree more than once while walking levels, so computed hashes are
/// cached by `(level, index)`.
#[derive(Debug)]
struct NodeHashCalculator {
    cache: HashMap<(usize, u64), B256>,
    leaves: Vec<B256>,
    first_leaf_index: u64,
    last_non_zero_indices: Vec<u64>,
    zeros: Vec<B256>,
}

impl NodeHashCalculator {
    fn new(leaves: Vec<B256>, first_leaf_index: u64, zeros: Vec<B256>) -> Self {
        let levels = zeros.len();
        let mut last_non_zero_indices = vec![0; levels];

        let mut last_index = first_leaf_index + leaves.len() as u64 - 1;
        for last_non_zero_index in &mut last_non_zero_indices {
            *last_non_zero_index = last_index;
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

    fn node_hash(&mut self, level: usize, index: u64) -> B256 {
        if let Some(cached) = self.cache.get(&(level, index)) {
            return *cached;
        }

        let hash = self.node_hash_internal(level, index);

        self.cache.insert((level, index), hash);
        hash
    }

    fn node_hash_internal(&mut self, level: usize, index: u64) -> B256 {
        assert!(level < self.zeros.len());

        if index > self.last_non_zero_indices[level] {
            return self.zeros[level];
        }

        if level == 0 {
            let range = self.first_leaf_index..(self.first_leaf_index + self.leaves.len() as u64);
            assert!(range.contains(&index));

            return self.leaves[(index - self.first_leaf_index) as usize];
        }

        let left_child_hash = self.node_hash(level - 1, index * 2);
        let right_child_hash = self.node_hash(level - 1, index * 2 + 1);

        keccak256([left_child_hash.0, right_child_hash.0].concat())
    }
}

/// Mirrors a `Bytes32PushTree` append while keeping its compact `_sides` representation.
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

    let mut current_index = index;
    let mut current_level_hash = leaf;
    let mut updated_sides = false;
    for i in 0..levels {
        // A left child becomes the stored side used when a later right child arrives.
        let is_left = current_index % U256::from(2u32) == U256::ZERO;

        if is_left && !updated_sides {
            tree._sides[i] = current_level_hash;
            updated_sides = true;
        }

        // Missing right children use the zero subtree; right children use the remembered left side.
        current_level_hash = if is_left {
            keccak256([current_level_hash.0, tree._zeros[i].0].concat())
        } else {
            keccak256([tree._sides[i].0, current_level_hash.0].concat())
        };

        current_index /= U256::from(2u32);
    }

    tree._sides[levels] = current_level_hash;
}

#[derive(Debug, Clone)]
struct ChainAggProof {
    chain_id_leaf_proof: Vec<B256>,
    chain_id_leaf_proof_mask: U256,
}

/// Reads a source chain's path in MessageRoot at one settlement-layer block.
///
/// The path is historical because a later chain append would authenticate a different shared root.
async fn get_chain_log_proof(
    l2_chain_id: u64,
    settlement_block_number: u64,
    l1_provider: &DynProvider,
    message_root_address: Address,
) -> anyhow::Result<ChainAggProof> {
    let message_root = IMessageRoot::new(message_root_address, l1_provider.clone());
    let merkle_path_builder = message_root
        .getMerklePathForChain(U256::from(l2_chain_id))
        .block(settlement_block_number.into());
    let merkle_path_fut = merkle_path_builder
        .call()
        .into_future()
        .map_err(|e| anyhow::Error::from(e).context("getMerklePathForChain"));
    let chain_index_builder = message_root
        .chainIndex(U256::from(l2_chain_id))
        .block(settlement_block_number.into());
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

/// Encodes a chain-tree proof in the flat `bytes32[]` layout consumed by the on-chain verifier.
///
/// The prefix is `[block-or-batch number + path mask, settlement-layer chain id, metadata]`,
/// followed by the chain-tree siblings. The metadata marks this as the final proof segment.
fn chain_proof_vector(
    batch_or_block_number: u64,
    chain_agg_proof: ChainAggProof,
    sl_chain_id: u64,
) -> Vec<B256> {
    let sl_encoded_data = (U256::from(batch_or_block_number) << U256::from(128u32))
        + chain_agg_proof.chain_id_leaf_proof_mask;

    let mut chain_proof_vector = vec![
        B256::from(sl_encoded_data.to_be_bytes()),
        B256::from(U256::from(sl_chain_id).to_be_bytes()),
        proof_metadata(chain_agg_proof.chain_id_leaf_proof.len(), 0, true),
    ];
    chain_proof_vector.extend(chain_agg_proof.chain_id_leaf_proof);

    chain_proof_vector
}

/// Builds the batch-tree segment for `batch_number` from MessageRoot state and append events.
///
/// The tree is read immediately before `settlement_block_number`; matching
/// `AppendedChainBatchRoot` events from that block are then replayed in order. The returned words are
/// `[l1 timestamp, absolute leaf index, sibling path...]`; the separate length counts only the
/// sibling path because the outer proof metadata needs that value.
async fn batch_tree_proof(
    settlement_block_number: u64,
    l2_chain_id: u64,
    batch_number: u64,
    l1_provider: &DynProvider,
    message_root_address: Address,
) -> anyhow::Result<(Vec<B256>, u8)> {
    anyhow::ensure!(
        settlement_block_number > 0,
        "cannot reconstruct a MessageRoot batch proof at L1 block 0"
    );

    let message_root = IMessageRoot::new(message_root_address, l1_provider.clone());
    let tree_call_builder = message_root
        .getChainTree(U256::from(l2_chain_id))
        .block((settlement_block_number - 1).into());
    let tree_future = tree_call_builder
        .call()
        .into_future()
        .map_err(|e| anyhow::Error::from(e).context("getChainTree"));

    let filter = Filter::new()
        .from_block(settlement_block_number)
        .to_block(settlement_block_number)
        .event_signature(AppendedChainBatchRoot::SIGNATURE_HASH)
        .topic1(U256::from(l2_chain_id))
        .address(message_root_address);
    let logs_future = l1_provider
        .get_logs(&filter)
        .map_err(|e| anyhow::Error::from(e).context("get_logs for AppendedChainBatchRoot"));

    let (tree, logs) = futures::future::try_join(tree_future, logs_future).await?;

    let events = logs
        .into_iter()
        .map(|log| {
            AppendedChainBatchRoot::decode_log(&log.inner)
                .map(|event| event.data)
                .map_err(anyhow::Error::from)
                .context("decode AppendedChainBatchRoot log")
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let batch_leaf_padding: B256 = keccak256(b"zkSync:BatchLeaf");
    let batch_idx = events
        .iter()
        .position(|event| event.batchNumber == U256::from(batch_number))
        .ok_or_else(|| anyhow::anyhow!("Batch number {} not found in logs", batch_number))?;
    let absolute_batch_idx = tree._nextLeafIndex.to::<usize>() + batch_idx;

    // The timestamp is part of `MessageHashing.batchLeafHash`, so the proof must carry the exact
    // value emitted with the proven batch root.
    let proven_l1_timestamp = B256::from(events[batch_idx].l1Timestamp.to_be_bytes::<32>());
    let new_hashes: Vec<B256> = events
        .into_iter()
        .map(|event| {
            let preimage = [
                batch_leaf_padding.0,
                event.chainBatchRoot.0,
                event.batchNumber.to_be_bytes::<32>(),
                event.l1Timestamp.to_be_bytes::<32>(),
            ]
            .concat();
            keccak256(preimage)
        })
        .collect();

    let batch_proof = calculate_batch_tree_proof(tree, new_hashes, batch_idx);
    let batch_proof_len = batch_proof.len() as u8;

    // `_getProofData` reads the timestamp and leaf index before consuming the sibling path.
    let mut proof = vec![
        proven_l1_timestamp,
        B256::from(U256::from(absolute_batch_idx).to_be_bytes()),
    ];
    proof.extend(batch_proof);

    Ok((proof, batch_proof_len))
}

/// Opaque MessageRoot extension appended after a source batch's log proof.
pub(crate) struct MessageRootProofExtension {
    batch_proof_len: u8,
    words: Vec<B256>,
}

/// Reconstructs both MessageRoot aggregation segments at the source batch's L1 execution block.
pub(crate) async fn build_message_root_proof_extension(
    l2_chain_id: u64,
    batch_number: u64,
    settlement_block_number: u64,
    l1_provider: &DynProvider,
    message_root_address: Address,
) -> anyhow::Result<MessageRootProofExtension> {
    let chain_proof_fut = get_chain_log_proof(
        l2_chain_id,
        settlement_block_number,
        l1_provider,
        message_root_address,
    );
    let batch_proof_fut = batch_tree_proof(
        settlement_block_number,
        l2_chain_id,
        batch_number,
        l1_provider,
        message_root_address,
    );
    let l1_chain_id_fut = l1_provider
        .get_chain_id()
        .map_err(|err| anyhow::Error::from(err).context("get_chain_id (L1)"));

    let (chain_proof, (mut words, batch_proof_len), l1_chain_id) =
        futures::try_join!(chain_proof_fut, batch_proof_fut, l1_chain_id_fut)?;
    words.extend(chain_proof_vector(
        settlement_block_number,
        chain_proof,
        l1_chain_id,
    ));

    Ok(MessageRootProofExtension {
        batch_proof_len,
        words,
    })
}

/// Encodes the full contract-facing proof while keeping segment metadata in one place.
pub(crate) fn assemble_log_proof(
    mut log_leaf_proof: Vec<B256>,
    extension: Option<MessageRootProofExtension>,
) -> Vec<B256> {
    let (batch_proof_len, is_final_node, extension_words) = match extension {
        Some(extension) => (extension.batch_proof_len, false, extension.words),
        None => (0, true, Vec::new()),
    };
    let metadata = proof_metadata(log_leaf_proof.len(), batch_proof_len, is_final_node);

    let mut proof = Vec::with_capacity(1 + log_leaf_proof.len() + extension_words.len());
    proof.push(metadata);
    proof.append(&mut log_leaf_proof);
    proof.extend(extension_words);
    proof
}

fn proof_metadata(log_proof_len: usize, batch_proof_len: u8, is_final_node: bool) -> B256 {
    let mut metadata = [0u8; 32];
    metadata[0] = LOG_PROOF_SUPPORTED_METADATA_VERSION;
    metadata[1] = log_proof_len as u8;
    metadata[2] = batch_proof_len;
    metadata[3] = u8::from(is_final_node);
    metadata.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::b256;

    #[test]
    fn assembles_final_log_proof_metadata() {
        let log_proof = vec![B256::repeat_byte(1), B256::repeat_byte(2)];

        let proof = assemble_log_proof(log_proof.clone(), None);

        let mut expected_metadata = [0_u8; 32];
        expected_metadata[0] = LOG_PROOF_SUPPORTED_METADATA_VERSION;
        expected_metadata[1] = log_proof.len() as u8;
        expected_metadata[3] = 1;
        assert_eq!(proof, [vec![expected_metadata.into()], log_proof].concat());
    }

    #[test]
    fn assembles_message_root_extension_metadata() {
        let log_proof = vec![B256::repeat_byte(1)];
        let extension_words = vec![B256::repeat_byte(2), B256::repeat_byte(3)];
        let extension = MessageRootProofExtension {
            batch_proof_len: 4,
            words: extension_words.clone(),
        };

        let proof = assemble_log_proof(log_proof.clone(), Some(extension));

        let mut expected_metadata = [0_u8; 32];
        expected_metadata[0] = LOG_PROOF_SUPPORTED_METADATA_VERSION;
        expected_metadata[1] = log_proof.len() as u8;
        expected_metadata[2] = 4;
        assert_eq!(
            proof,
            [vec![expected_metadata.into()], log_proof, extension_words].concat()
        );
    }

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

                    // The reconstructed siblings must reach the same root as replaying every new
                    // leaf into the compact tree.
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
