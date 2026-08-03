//! Batch-boundary merkle tree update: proofs and intermediate hashes for the leaves the batch touches.

use super::*;
use anyhow::Context;

pub fn account_flat_key(account: Address) -> B256 {
    zisk_merkle::derive_account_properties_key(&account.into_array())
}

/// Re-anchor every block's read proofs to the batch pre-state tree.
///
/// The guest authenticates all reads against the L1-pinned `tree_root_before`
/// (the batch pre-state root). The per-block builder extracts each block's
/// proofs against that block's own intermediate pre-state, so a multi-block
/// batch whose later block first reads a key would fail closed against the
/// pinned root. Re-extract every block's storage proofs against the batch
/// pre-state tree, dedup keys across blocks (first occurrence wins), and skip
/// keys an earlier block wrote (the guest overlay serves those). Drop each
/// block's account preimages that it no longer proves, so the witness stays
/// consistent.
///
/// `write_keys_per_block` lists the flat keys each block writes, in block order.
pub(crate) fn reanchor_block_read_proofs(
    block_data_vec: &mut [ZiskBlockData],
    write_keys_per_block: &[Vec<B256>],
    batch_tree_start: &mut VersionedMerkleTree,
) {
    // Keys already proven somewhere in the batch witness, and keys an earlier
    // block wrote (served by the guest overlay, so no pre-state proof needed).
    let mut proven: HashSet<B256> = HashSet::new();
    let mut written: HashSet<B256> = HashSet::new();

    for (block_idx, block_data) in block_data_vec.iter_mut().enumerate() {
        let keys: Vec<B256> = block_data
            .block_input
            .storage_proofs
            .iter()
            .map(|(key, _)| *key)
            .collect();
        let mut new_proofs = Vec::with_capacity(keys.len());
        let mut proved_here: HashSet<B256> = HashSet::new();
        for key in keys {
            if proven.contains(&key) || written.contains(&key) {
                continue;
            }
            new_proofs.push((key, extract_proof(batch_tree_start, key)));
            proven.insert(key);
            proved_here.insert(key);
        }
        block_data.block_input.storage_proofs = new_proofs;

        // Keep only the account preimages whose leaf proof this block still
        // carries. An account proven by an earlier block, or written earlier,
        // is served from the accumulated guest state.
        block_data
            .block_input
            .account_preimages
            .retain(|(addr, _)| proved_here.contains(&account_flat_key(*addr)));

        // The guest applies this block's writes to its overlay, so later blocks
        // read them without a pre-state proof.
        if let Some(writes) = write_keys_per_block.get(block_idx) {
            written.extend(writes.iter().copied());
        }
    }
}

pub(super) fn extract_proof(tree: &mut VersionedMerkleTree, flat_key: B256) -> StorageProof {
    if let Some(tree_index) = tree.tree_index(flat_key) {
        let p = tree.merkle_proof(tree_index);
        StorageProof::Existing(SlotProofEntry {
            index: p.index,
            value: B256::from(p.leaf.value.as_u8_array()),
            next_index: p.leaf.next,
            siblings: p.path.iter().map(|h| B256::from(h.as_u8_array())).collect(),
        })
    } else {
        let prev_index = tree.prev_tree_index(flat_key);
        let left = tree.merkle_proof(prev_index);
        let right = tree.merkle_proof(left.leaf.next);
        StorageProof::NonExisting {
            left_neighbor: NeighborProofEntry {
                entry: SlotProofEntry {
                    index: left.index,
                    value: B256::from(left.leaf.value.as_u8_array()),
                    next_index: left.leaf.next,
                    siblings: left
                        .path
                        .iter()
                        .map(|h| B256::from(h.as_u8_array()))
                        .collect(),
                },
                leaf_key: B256::from(left.leaf.key.as_u8_array()),
            },
            right_neighbor: NeighborProofEntry {
                entry: SlotProofEntry {
                    index: right.index,
                    value: B256::from(right.leaf.value.as_u8_array()),
                    next_index: right.leaf.next,
                    siblings: right
                        .path
                        .iter()
                        .map(|h| B256::from(h.as_u8_array()))
                        .collect(),
                },
                leaf_key: B256::from(right.leaf.key.as_u8_array()),
            },
        }
    }
}

#[derive(Clone)]
pub(super) struct LeafWithProof {
    index: u64,
    key: B256,
    value: B256,
    next_index: u64,
    siblings: Vec<B256>, // 64 entries
}

pub(super) fn get_leaf_proof(tree: &mut VersionedMerkleTree, idx: u64) -> LeafWithProof {
    let p = tree.merkle_proof(idx);
    LeafWithProof {
        index: p.index,
        key: B256::from(p.leaf.key.as_u8_array()),
        value: B256::from(p.leaf.value.as_u8_array()),
        next_index: p.leaf.next,
        siblings: p.path.iter().map(|h| B256::from(h.as_u8_array())).collect(),
    }
}

/// Build the pre-state witness the guest recomputes the post-state root from.
///
/// A populated tree holds the MIN and MAX guard leaves, so the traversal relies
/// on a positive `leaf_count` and on a predecessor for every inserted key.
pub(crate) fn build_tree_update(
    tree: &mut VersionedMerkleTree,
    writes: &[zksync_os_interface::types::StorageWrite],
    leaf_count: u64,
) -> anyhow::Result<BatchTreeUpdate> {
    anyhow::ensure!(
        leaf_count > 0,
        "pre-state tree is empty; it has no guard leaves to anchor the update against"
    );
    let mut operations = Vec::new();
    let mut entries = Vec::new();
    let mut leaf_proofs: HashMap<u64, LeafWithProof> = HashMap::new();

    // Track in-memory linked list for correct insert ordering.
    // Maps key → (index, next_index) for both existing and newly inserted leaves.
    let mut key_to_index: BTreeMap<B256, u64> = BTreeMap::new();
    let mut index_to_next: HashMap<u64, u64> = HashMap::new();
    let mut next_free_index = leaf_count;

    // Seed the key map from leaves we discover.
    // Only load proofs for indices that actually exist in the tree (< leaf_count).
    let ensure_leaf = |tree: &mut VersionedMerkleTree,
                       idx: u64,
                       leaf_proofs: &mut HashMap<u64, LeafWithProof>,
                       key_to_index: &mut BTreeMap<B256, u64>,
                       index_to_next: &mut HashMap<u64, u64>| {
        if idx < leaf_count && !leaf_proofs.contains_key(&idx) {
            let p = get_leaf_proof(tree, idx);
            key_to_index.insert(p.key, p.index);
            index_to_next.insert(p.index, p.next_index);
            leaf_proofs.insert(idx, p);
        }
    };

    for write in writes {
        let flat_key = write.key;

        if let Some(tree_index) = tree.tree_index(flat_key) {
            // Update existing leaf
            operations.push(WriteOp::Update { index: tree_index });
            entries.push((flat_key, write.value));
            ensure_leaf(
                tree,
                tree_index,
                &mut leaf_proofs,
                &mut key_to_index,
                &mut index_to_next,
            );
        } else {
            // Insert new leaf. Load the tree's own predecessor first so the
            // ordered in-memory map always contains it; the true predecessor
            // is then the largest key below the new key across existing AND
            // pending-inserted leaves. (Taking the in-memory candidate alone
            // is wrong: any already-loaded smaller key — e.g. a leaf touched
            // by an earlier update — would shadow the tree's predecessor and
            // corrupt the linked list.)
            let tree_prev = tree.prev_tree_index(flat_key);
            ensure_leaf(
                tree,
                tree_prev,
                &mut leaf_proofs,
                &mut key_to_index,
                &mut index_to_next,
            );
            let prev_index = *key_to_index
                .range(..flat_key)
                .next_back()
                .with_context(|| {
                    format!("no predecessor leaf below key {flat_key} (MIN guard leaf missing)")
                })?
                .1;
            let old_next = index_to_next.get(&prev_index).copied().with_context(|| {
                format!("linked-list successor of leaf {prev_index} is unknown")
            })?;
            ensure_leaf(
                tree,
                old_next,
                &mut leaf_proofs,
                &mut key_to_index,
                &mut index_to_next,
            );

            let this_index = next_free_index;
            next_free_index += 1;

            operations.push(WriteOp::Insert { prev_index });
            entries.push((flat_key, write.value));

            // Update in-memory linked list: prev → this → old_next
            index_to_next.insert(prev_index, this_index);
            index_to_next.insert(this_index, old_next);
            key_to_index.insert(flat_key, this_index);
        }
    }

    // Anchor planning: the guest's new-root pass needs a sibling for every
    // node off the new paths. Siblings inside the pre-existing tree that the
    // old-root pass would not authenticate must be covered by an *anchor*
    // leaf (the leftmost leaf of that subtree) included in the witness, so
    // that the old-root pass authenticates the region. Iterate to a fixpoint:
    // each added anchor changes both traversals.
    let insert_count = operations
        .iter()
        .filter(|op| matches!(op, WriteOp::Insert { .. }))
        .count() as u64;
    loop {
        let mut old_indices: Vec<u64> = leaf_proofs.keys().copied().collect();
        old_indices.sort_unstable();
        let authenticated = simulate_pass1_authenticated(&old_indices, leaf_count);

        let mut new_indices: Vec<u64> = old_indices;
        new_indices.extend(leaf_count..leaf_count + insert_count);
        new_indices.sort_unstable();
        new_indices.dedup();

        let missing = simulate_pass2_missing(&new_indices, leaf_count, &authenticated);
        if missing.is_empty() {
            break;
        }
        for (depth, sibling) in missing {
            // Leftmost leaf of the uncovered subtree; < leaf_count by construction.
            let anchor = sibling << depth;
            ensure_leaf(
                tree,
                anchor,
                &mut leaf_proofs,
                &mut key_to_index,
                &mut index_to_next,
            );
        }
    }

    // Build sorted_leaves (touched leaves + anchors)
    let mut sorted_leaves: Vec<(u64, TreeLeaf)> = leaf_proofs
        .values()
        .map(|p| {
            (
                p.index,
                TreeLeaf {
                    key: p.key,
                    value: p.value,
                    next_index: p.next_index,
                },
            )
        })
        .collect();
    sorted_leaves.sort_by_key(|(idx, _)| *idx);

    let old_leaf_indices: Vec<u64> = sorted_leaves.iter().map(|(idx, _)| *idx).collect();
    let intermediate_hashes =
        compute_old_intermediate_hashes(&old_leaf_indices, leaf_count, &leaf_proofs, tree);

    tracing::debug!(
        leaves = old_leaf_indices.len(),
        hashes = intermediate_hashes.len(),
        leaf_count_before = leaf_count,
        inserts = insert_count,
        "tree update witness built"
    );

    Ok(BatchTreeUpdate {
        operations,
        entries,
        sorted_leaves,
        intermediate_hashes,
        leaf_count_before: leaf_count,
    })
}

/// Mirror of the guest's old-root pass (`zip_and_record`), positions only:
/// returns every (depth, index) the pass authenticates — leaf hashes,
/// consumed siblings, and computed internal nodes.
///
/// `leaf_count` is positive: [`build_tree_update`] is the only caller and it
/// rejects an empty pre-state tree.
pub(super) fn simulate_pass1_authenticated(
    old_indices: &[u64],
    leaf_count: u64,
) -> HashSet<(u8, u64)> {
    let mut authenticated: HashSet<(u8, u64)> = old_indices.iter().map(|idx| (0u8, *idx)).collect();
    let mut nodes: Vec<u64> = old_indices.to_vec();
    let mut last_idx_on_level = leaf_count - 1;

    for depth in 0..TREE_DEPTH {
        let mut next_level = Vec::new();
        let mut i = 0;
        while i < nodes.len() {
            let idx = nodes[i];
            if idx % 2 == 1 {
                authenticated.insert((depth, idx - 1));
                i += 1;
            } else if nodes.get(i + 1).copied() == Some(idx + 1) {
                i += 2;
            } else {
                if idx != last_idx_on_level {
                    authenticated.insert((depth, idx + 1));
                }
                i += 1;
            }
            next_level.push(idx / 2);
            authenticated.insert((depth + 1, idx / 2));
        }
        nodes = next_level;
        last_idx_on_level /= 2;
    }
    authenticated
}

/// Mirror of the guest's new-root pass: returns the (depth, index) of every
/// sibling that is neither computed, nor authenticated by the old-root pass,
/// nor provably empty (subtree entirely at or beyond `leaf_count_before`).
pub(super) fn simulate_pass2_missing(
    new_indices: &[u64],
    leaf_count_before: u64,
    authenticated: &HashSet<(u8, u64)>,
) -> Vec<(u8, u64)> {
    let mut missing = Vec::new();
    let mut nodes: Vec<u64> = new_indices.to_vec();

    for depth in 0..TREE_DEPTH {
        let mut next_level = Vec::new();
        let mut i = 0;
        while i < nodes.len() {
            let idx = nodes[i];
            let sibling = idx ^ 1;
            if nodes.get(i + 1).copied() == Some(sibling) {
                i += 2;
            } else {
                i += 1;
                if !authenticated.contains(&(depth, sibling))
                    && (sibling << depth) < leaf_count_before
                {
                    missing.push((depth, sibling));
                }
            }
            next_level.push(idx / 2);
        }
        nodes = next_level;
    }
    missing
}

/// Intermediate sibling hashes for the guest's old-root pass, in traversal
/// order, resolved from already-loaded leaf proofs (falling back to loading a
/// proof from the needed subtree).
///
/// `old_leaf_count` is positive: [`build_tree_update`] is the only caller and
/// it rejects an empty pre-state tree. Every proof carries one sibling per
/// tree level, and the guest lib's empty-subtree table covers depths
/// `0..=TREE_DEPTH`, so both indexed reads below stay in range.
pub(super) fn compute_old_intermediate_hashes(
    old_indices: &[u64],
    old_leaf_count: u64,
    leaf_proofs: &HashMap<u64, LeafWithProof>,
    tree: &mut VersionedMerkleTree,
) -> Vec<B256> {
    let empty_hashes = zisk_merkle::empty_subtree_hashes_vec();

    // Build sibling cache from already-loaded proofs.
    // Key: (depth, node_index_at_that_depth) -> hash of that node
    let mut sibling_cache: HashMap<(u8, u64), B256> = HashMap::new();
    for proof in leaf_proofs.values() {
        for d in 0..TREE_DEPTH {
            let sibling_node = (proof.index >> d) ^ 1;
            sibling_cache
                .entry((d, sibling_node))
                .or_insert(proof.siblings[d as usize]);
        }
    }

    let mut resolve_sibling = |depth: u8, sibling_node: u64| -> B256 {
        if let Some(&h) = sibling_cache.get(&(depth, sibling_node)) {
            return h;
        }
        let range_start = sibling_node << depth;
        if range_start >= old_leaf_count {
            return empty_hashes[depth as usize];
        }
        let leaf_in_subtree = range_start.min(old_leaf_count.saturating_sub(1));
        let p = get_leaf_proof(tree, leaf_in_subtree);
        for d in 0..TREE_DEPTH {
            let sn = (p.index >> d) ^ 1;
            sibling_cache
                .entry((d, sn))
                .or_insert(p.siblings[d as usize]);
        }
        if let Some(&h) = sibling_cache.get(&(depth, sibling_node)) {
            return h;
        }
        empty_hashes[depth as usize]
    };

    let mut old_hashes = Vec::new();
    let mut node_indices: Vec<u64> = old_indices.to_vec();
    let mut last_idx = old_leaf_count - 1;
    for depth in 0..TREE_DEPTH {
        let mut i = 0;
        let mut next_level = Vec::new();
        while i < node_indices.len() {
            let idx = node_indices[i];
            if idx % 2 == 1 {
                old_hashes.push(resolve_sibling(depth, idx - 1));
                next_level.push(idx / 2);
                i += 1;
            } else if node_indices.get(i + 1).copied() == Some(idx + 1) {
                next_level.push(idx / 2);
                i += 2;
            } else {
                if idx != last_idx {
                    old_hashes.push(resolve_sibling(depth, idx + 1));
                }
                next_level.push(idx / 2);
                i += 1;
            }
        }
        node_indices = next_level;
        last_idx /= 2;
    }
    old_hashes
}

// ---------------------------------------------------------------------------
// Interop slot proofs
// ---------------------------------------------------------------------------

/// SystemContext, address `0x800b`.
const SYSTEM_CONTEXT_ADDRESS: [u8; 20] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x80, 0x0b,
];
/// MessageRoot (L2 message-root aggregator), address `0x10005`.
const MESSAGE_ROOT_ADDRESS: [u8; 20] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0x00, 0x05,
];

/// Build the three slot proofs the guest reproduces `read_batch_context_inputs`
/// from (`executor::interop`):
/// - `sl_chain_id`: SystemContext (`0x800b`) slot 0.
/// - `multichain_root`: MessageRoot (`0x10005`) slot `0x04` (aggregation height)
///   and `nodes[height][0]`.
///
/// All three slots are proven against the POST-batch tree view. The guest reads
/// `sl_chain_id` at post-state for every batch, so it derives the value even in
/// an upgrade batch that writes the slot. `multichain_root` is post-state as
/// well, matching native `read_batch_context_inputs` timing.
///
/// Mirrors `lib/storage_api/src/read_multichain_root.rs` for the slot layout;
/// the height is read from its own proof so the second slot can be derived.
pub(crate) fn build_interop_slot_proofs(post_tree: &mut VersionedMerkleTree) -> InteropSlotProofs {
    use alloy::primitives::{U256, keccak256};

    // sl_chain_id: SystemContext 0x800b slot 0, post-state.
    let sl_key = zisk_merkle::derive_flat_storage_key(&SYSTEM_CONTEXT_ADDRESS, &B256::ZERO);
    let sl_chain_id = extract_proof(post_tree, sl_key);

    // multichain aggregation-tree height: MessageRoot 0x10005 slot 0x04, post-state.
    let height_key =
        zisk_merkle::derive_flat_storage_key(&MESSAGE_ROOT_ADDRESS, &B256::with_last_byte(0x04));
    let multichain_height = extract_proof(post_tree, height_key);
    let height = match &multichain_height {
        StorageProof::Existing(e) => e.value,
        StorageProof::NonExisting { .. } => B256::ZERO,
    };

    // multichain root: nodes[height][0] = keccak256( keccak256(word(0x06)) + height ), post-state.
    let base = U256::from_be_bytes(keccak256(B256::with_last_byte(0x06).as_slice()).0);
    let node_slot = base.wrapping_add(U256::from_be_bytes(height.0));
    let root_slot = keccak256(node_slot.to_be_bytes::<32>());
    let root_key = zisk_merkle::derive_flat_storage_key(&MESSAGE_ROOT_ADDRESS, &root_slot);
    let multichain_root = extract_proof(post_tree, root_key);

    InteropSlotProofs {
        sl_chain_id,
        multichain_height,
        multichain_root,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
