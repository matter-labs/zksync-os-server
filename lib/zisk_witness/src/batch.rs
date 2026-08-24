//! Per-batch ZiSK `BatchInput` assembly for the second proof lane.
//!
//! [`assemble_batch_input`] folds the per-block second proof-system data of a
//! whole batch into one batch-level `BatchInput`, builds the authenticated
//! pre-state tree update, and serializes the result with the guest's wire
//! config. The batcher seal path calls it once per sealed batch.

use alloy::consensus::BlobTransactionSidecar;
use alloy::primitives::{Address, B256};
use anyhow::Context;
use blake2::{Blake2s256, Digest};
use zksync_os_batch_types::{BlockMerkleTreeData, PendingBatchInfo};
use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper};
use zksync_os_native_pig::tree::VersionedMerkleTree;
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord};
use zksync_os_types::{BlockOutput, PubdataMode};
use zksync_os_zisk_lib::types::*;

use crate::input_builder::{
    ZiskBlockData, build_interop_slot_proofs, build_tree_update, reanchor_block_read_proofs,
    recover_code_matching, spec_id_from_execution_version,
};

/// Chain-config parameters committed into the ZiSK batch public input
/// (`chain_config_hash` preimage, together with the chain id).
#[derive(Clone, Copy, Debug)]
pub struct ZiskChainConfig {
    pub fri_proof_verification_enabled: bool,
    pub max_tx_gas_limit: u64,
}

/// The two batch-boundary tree views the ZiSK batch assembly needs: the tree
/// before the first block of the batch, and after the last block. Built inside
/// the crate by [`batch_tree_views`] on the batcher's enabled seal path.
pub(crate) struct BatchTreeViews {
    pub(crate) start: VersionedMerkleTree,
    pub(crate) end: VersionedMerkleTree,
}

/// One block's inputs as the batch assembly reads them: the native block
/// output and its replay record. Earlier revisions also carried the block's
/// tree update and the Airbender prover input; nothing in this crate ever read
/// them.
type BatchBlock<'a> = (&'a BlockOutput, &'a ReplayRecord);

/// One block of the batch with the witness built for it.
pub(crate) struct AssembledBlock<'a> {
    pub output: &'a BlockOutput,
    pub record: &'a ReplayRecord,
    pub witness: ZiskBlockData,
}

/// One block as the seal path sees it: the native output, its replay record,
/// and the tree update proof captured when the block was applied.
pub struct ZiskSealBlock<'a> {
    pub output: &'a BlockOutput,
    pub record: &'a ReplayRecord,
    pub tree_data: &'a BlockMerkleTreeData,
}

/// The batch-level values the guest commits to, unchanged from seal through
/// assembly.
pub struct BatchWitnessContext<'a> {
    pub pubdata_mode: PubdataMode,
    pub multichain_root: B256,
    pub sl_chain_id: u64,
    pub batch_info: &'a PendingBatchInfo,
    pub blob_sidecar: Option<&'a BlobTransactionSidecar>,
    pub chain_config: ZiskChainConfig,
}

/// Build a whole batch's ZiSK `BatchInput` at seal time, where the block
/// outputs, replay records and both tree boundaries are all in hand.
///
/// The touched-key sets come from each block's tree update: witness discovery
/// is checked against them, so a gap fails here rather than at proving time.
pub fn build_batch_witness(
    blocks: &[ZiskSealBlock<'_>],
    read_state: &impl ReadStateHistory,
    merkle_tree: &MerkleTree<RocksDBWrapper>,
    context: BatchWitnessContext<'_>,
) -> anyhow::Result<Vec<u8>> {
    let mut per_block: Vec<AssembledBlock<'_>> = Vec::with_capacity(blocks.len());
    for block in blocks {
        let block_number = block.record.block_context.block_number;
        // The pre-state view this block executed against.
        let tree = VersionedMerkleTree::new(merkle_tree.clone(), block_number - 1);
        let touched: Vec<B256> = block
            .tree_data
            .read_keys
            .iter()
            .chain(block.tree_data.written_keys.iter())
            .copied()
            .collect();
        let data = crate::input_builder::build_block_witness(
            block.output,
            block.record,
            tree,
            &touched,
            read_state,
        )
        .with_context(|| format!("build the ZiSK witness for block {block_number}"))?;
        per_block.push(AssembledBlock {
            output: block.output,
            record: block.record,
            witness: data,
        });
    }

    assemble_batch(per_block, read_state, merkle_tree, context)
}

/// Gather the batch-level witness inputs the guest needs — the tree views at
/// both batch boundaries, the after-state 0x8003 account-properties preimages,
/// and the bytecodes those preimages reference — then fold the per-block
/// witnesses into one `BatchInput`.
fn assemble_batch(
    assembled: Vec<AssembledBlock<'_>>,
    read_state: &impl ReadStateHistory,
    merkle_tree: &MerkleTree<RocksDBWrapper>,
    context: BatchWitnessContext<'_>,
) -> anyhow::Result<Vec<u8>> {
    let blocks: Vec<BatchBlock<'_>> = assembled
        .iter()
        .map(|block| (block.output, block.record))
        .collect();
    let blocks = blocks.as_slice();
    let tree_views = batch_tree_views(merkle_tree, blocks)?;
    let account_preimages_after = extract_account_preimages_after(blocks, read_state)?;
    let referenced_bytecodes =
        recover_referenced_bytecodes(&account_preimages_after, read_state, blocks)?;

    assemble_batch_input(
        assembled,
        context,
        tree_views,
        account_preimages_after,
        referenced_bytecodes,
    )
}

/// Fold the per-block witnesses into one batch-level `BatchInput`, serialized
/// with the guest's wire config.
fn assemble_batch_input(
    assembled: Vec<AssembledBlock<'_>>,
    context: BatchWitnessContext<'_>,
    tree_views: BatchTreeViews,
    account_preimages_after: Vec<(Address, Vec<u8>)>,
    referenced_bytecodes: Vec<(B256, Vec<u8>)>,
) -> anyhow::Result<Vec<u8>> {
    let BatchWitnessContext {
        pubdata_mode,
        multichain_root,
        sl_chain_id,
        batch_info,
        blob_sidecar,
        chain_config: zisk_chain_config,
    } = context;
    let BatchTreeViews {
        start: mut batch_tree_start,
        end: batch_tree_end,
    } = tree_views;

    let mut blocks: Vec<BatchBlock<'_>> = Vec::with_capacity(assembled.len());
    let mut block_data_vec: Vec<ZiskBlockData> = Vec::with_capacity(assembled.len());
    for block in assembled {
        blocks.push((block.output, block.record));
        block_data_vec.push(block.witness);
    }
    let blocks = blocks.as_slice();

    // Batch pre-state values, captured before the read-proof re-anchoring below
    // mutably borrows `block_data_vec`.
    let first_block_data = block_data_vec
        .first()
        .context("batch carries no ZiSK block data")?;
    let batch_tree_root_before = first_block_data.tree_root_before;
    let leaf_count_before = first_block_data.leaf_count_before;
    let block_number_before = first_block_data.block_number_before;
    let last_block_timestamp_before = first_block_data.previous_block_timestamp;
    let first_replay = &blocks.first().context("batch has no blocks")?.1;
    let first_ctx = &first_replay.block_context;
    let da_scheme = pubdata_mode.da_commitment_scheme() as u8;

    // The guest recomputes the batch's DA commitment, so it needs the real
    // pubdata bytes; a length-only block cannot be proved.
    let mut pubdata: Vec<u8> = Vec::new();
    for (block_output, replay_record) in blocks {
        let bytes = block_output.pubdata.bytes().with_context(|| {
            format!(
                "block {} kept only its pubdata length; the second proof system needs the bytes",
                replay_record.block_context.block_number
            )
        })?;
        pubdata.extend_from_slice(bytes);
    }

    // Compute block_hashes_blake for the state BEFORE the first block in this batch.
    // This must match the state commitment preimage used by the server/L1:
    //   Blake2s(previous_255_block_hashes || current_block_hash)
    // where "previous_255" are hashes of the 255 blocks before the "current" block,
    // and "current_block_hash" is the hash of the block that produced this state.
    //
    // The block_context.block_hashes array has block_hashes[N] = hash of block (current - N - 1).
    // For the "before" state at block B, the "current block" that produced this state is block B-1.
    // The genesis state uses: Blake2s(255 * [0; 32] || genesis_header_hash).
    //
    // We need to reconstruct the same ordering: the first 255 entries are the hashes
    // BEFORE the previous block (indices 1..255 of block_hashes), and the last entry
    // is block_hashes[0] (the previous block's hash, which IS the "current" for the state).
    //
    // However, block_hashes_for_first_block() puts genesis at index 255, not index 0.
    // So for block 1, block_hashes[0] = 0 and block_hashes[255] = genesis_hash.
    // The state commitment uses: Blake2s(0, 0, ..., 0, genesis_hash) with genesis_hash LAST.
    // We need to match that: hash all 256 entries in order [0, 1, 2, ..., 255].
    let block_hashes_blake_before = {
        let mut hasher = Blake2s256::new();
        for hash in &first_ctx.block_hashes.0 {
            hasher.update(hash.to_be_bytes::<32>());
        }
        B256::from_slice(&hasher.finalize())
    };

    // Use the LAST block's context for previous_block_hashes — this feeds into
    // block_hashes_blake_after in the executor, which must match the server's
    // state commitment that uses last_block_context.block_hashes.0[1..].
    let last_replay = &blocks.last().context("batch has no blocks")?.1;
    let last_ctx = &last_replay.block_context;
    let previous_block_hashes: Vec<B256> = last_ctx.block_hashes.0[1..]
        .iter()
        .map(|h| B256::from(h.to_be_bytes::<32>()))
        .collect();

    let upgrade_tx_hash = batch_info.upgrade_tx_hash.unwrap_or(B256::ZERO);

    let spec_id = match spec_id_from_execution_version(first_ctx.execution_version)? {
        zksync_os_revm::ZkSpecId::AtlasV1 => 0u8,
        zksync_os_revm::ZkSpecId::AtlasV2 => 1u8,
        zksync_os_revm::ZkSpecId::AtlasV3 => 2u8,
        // The pinned guest build maps only spec ids 0..=2 and panics on
        // anything else, so a newer spec must fail the witness build here
        // rather than produce an input the guest cannot execute.
        spec => anyhow::bail!(
            "the second proof system's guest build does not support {spec:?}; \
             update the pinned `zksync-os-zisk` before proving batches at this spec"
        ),
    };

    // Authenticated interop slot proofs. The guest derives `sl_chain_id`
    // and `multichain_root` from these (executor::interop) instead of trusting
    // the witness scalars. Required for v31+ (v30 commits neither). All slots
    // are proven against the post-batch tree, matching native
    // `read_batch_context_inputs` timing and letting the guest derive
    // `sl_chain_id` even in an upgrade batch that writes it.
    let interop_proofs = if first_replay.protocol_version.minor >= 31 {
        let mut post = batch_tree_end;
        Some(build_interop_slot_proofs(&mut post))
    } else {
        None
    };

    // Re-anchor every block's read proofs to the batch pre-state tree, so the
    // guest can authenticate all reads against the pinned `tree_root_before`.
    // A later block that first reads a key gets a pre-batch proof; keys written
    // by an earlier block are served by the guest overlay. This must run before
    // `build_batch_tree_update`, which consumes `batch_tree_start`.
    let write_keys_per_block: Vec<Vec<B256>> = blocks
        .iter()
        .map(|(block_output, _)| block_output.storage_writes.iter().map(|w| w.key).collect())
        .collect();
    reanchor_block_read_proofs(
        &mut block_data_vec,
        &write_keys_per_block,
        &mut batch_tree_start,
    );
    let tree_update = build_batch_tree_update(blocks, batch_tree_start)?;

    let batch_input = BatchInput {
        version: zksync_os_zisk_lib::types::BATCH_INPUT_VERSION,
        chain_id: first_ctx.chain_id,
        spec_id,
        protocol_version_minor: first_replay.protocol_version.minor as u32,
        batch_meta: BatchMeta {
            tree_root_before: batch_tree_root_before,
            leaf_count_before,
            block_number_before,
            last_block_timestamp_before,
            block_hashes_blake_before,
            previous_block_hashes,
            upgrade_tx_hash,
            da_commitment_scheme: da_scheme,
            pubdata,
            multichain_root,
            sl_chain_id,
            blob_versioned_hashes: blob_sidecar
                .as_ref()
                .map(|sidecar| {
                    sidecar
                        .commitments
                        .iter()
                        .map(|commitment| {
                            alloy::eips::eip4844::kzg_to_versioned_hash(commitment.as_slice())
                        })
                        .collect()
                })
                .unwrap_or_default(),
            tree_update,
            account_preimages_after,
            fri_proof_verification_enabled: zisk_chain_config.fri_proof_verification_enabled,
            max_tx_gas_limit: zisk_chain_config.max_tx_gas_limit,
            interop_proofs,
        },
        blocks: block_data_vec
            .iter()
            .map(|d| {
                let mut bi = d.block_input.clone();
                // All reads are proven against the batch pre-state root, so the
                // guest's `validate_expected_tree_roots` accepts this value for
                // every block.
                bi.expected_tree_root = batch_tree_root_before;
                bi
            })
            .collect(),
        bytecodes: {
            let mut seen = std::collections::HashSet::new();
            let mut all_bytecodes = Vec::new();
            for d in &block_data_vec {
                for (hash, code) in &d.bytecodes {
                    if seen.insert(*hash) {
                        all_bytecodes.push((*hash, code.clone()));
                    }
                }
            }
            for (hash, code) in &referenced_bytecodes {
                if seen.insert(*hash) {
                    all_bytecodes.push((*hash, code.clone()));
                }
            }
            all_bytecodes
        },
    };

    // Serialize with the guest's wire config (bincode 2.x, standard: little-
    // endian bytes and variable-length integers) through the same `wire::encode`
    // the guest ELF decodes with. This keeps the batch input byte-for-byte
    // identical on both sides. See `zksync_os_zisk_lib::wire`.
    zksync_os_zisk_lib::wire::encode(&batch_input).context("encode the ZiSK batch input")
}

/// Build a batch-level tree update from the batch-start tree view: pre-state
/// leaf proofs (touched leaves + anchors) and old-root sibling hashes. The
/// guest recomputes the new root from that authenticated pre-state and the
/// REVM-verified writes alone — no post-state data is shipped at all.
fn build_batch_tree_update(
    blocks: &[BatchBlock<'_>],
    mut batch_tree_start: VersionedMerkleTree,
) -> anyhow::Result<Option<zksync_os_zisk_lib::merkle::BatchTreeUpdate>> {
    // Collect all storage writes across all blocks, deduplicate (last-writer-wins per key)
    let mut combined_writes: Vec<zksync_os_interface::types::StorageWrite> = Vec::new();
    let mut seen_keys: std::collections::HashMap<B256, usize> = std::collections::HashMap::new();
    for (block_output, _) in blocks {
        for write in &block_output.storage_writes {
            let key = B256::from(write.key);
            if let Some(&pos) = seen_keys.get(&key) {
                combined_writes[pos] = write.clone();
            } else {
                seen_keys.insert(key, combined_writes.len());
                combined_writes.push(write.clone());
            }
        }
    }

    if combined_writes.is_empty() {
        return Ok(None);
    }

    let leaf_count = batch_tree_start.root_info()?.1;

    Ok(Some(build_tree_update(
        &mut batch_tree_start,
        &combined_writes,
        leaf_count,
    )?))
}

/// Build the two batch-boundary tree views the batch assembly needs: the tree
/// before the first block of the batch, and after the last block (the
/// post-batch state that `read_multichain_root` and the interop slot proofs
/// read). [`assemble_batch`] runs only on the batcher's enabled seal path with a
/// non-empty batch that starts above the genesis block, so the views are always
/// available. An error here degrades this batch's ZiSK data.
fn batch_tree_views(
    merkle_tree: &MerkleTree<RocksDBWrapper>,
    blocks: &[BatchBlock<'_>],
) -> anyhow::Result<BatchTreeViews> {
    let first_block_number = blocks
        .first()
        .context("batch has no blocks")?
        .1
        .block_context
        .block_number;
    let last_block_number = blocks
        .last()
        .context("batch has no blocks")?
        .1
        .block_context
        .block_number;
    // The pre-state view reads the tree one version below the first block.
    let pre_state_version = first_block_number
        .checked_sub(1)
        .context("batch starts at the genesis block, which has no pre-state tree version")?;
    Ok(BatchTreeViews {
        start: VersionedMerkleTree::new(merkle_tree.clone(), pre_state_version),
        end: VersionedMerkleTree::new(merkle_tree.clone(), last_block_number),
    })
}

/// Extract the after-state account preimages for 0x8003 verification. Reads
/// the state view after the batch's last block. Second proof-system only.
fn extract_account_preimages_after(
    blocks: &[BatchBlock<'_>],
    read_state: &impl ReadStateHistory,
) -> anyhow::Result<Vec<(Address, Vec<u8>)>> {
    use zksync_os_interface::traits::{PreimageSource, ReadStorage};
    let last_block_number = blocks
        .last()
        .context("batch has no blocks")?
        .1
        .block_context
        .block_number;
    let mut state_after = read_state.state_view_at(last_block_number)?;
    let mut seen = std::collections::HashSet::new();
    let mut preimages = Vec::new();
    let add = |addr: Address,
               state_after: &mut _,
               seen: &mut std::collections::HashSet<Address>,
               preimages: &mut Vec<(Address, Vec<u8>)>| {
        if !seen.insert(addr) {
            return;
        }
        let addr_bytes: [u8; 20] = addr.into();
        let flat_key = zksync_os_zisk_lib::merkle::derive_account_properties_key(&addr_bytes);
        if let Some(hash_value) =
            ReadStorage::read(state_after, alloy::primitives::B256::from(flat_key.0))
            && let Some(preimage) = PreimageSource::get_preimage(state_after, hash_value)
        {
            preimages.push((addr, preimage));
        }
    };
    const ACCOUNT_PROPERTIES_ADDRESS: Address =
        alloy::primitives::address!("0000000000000000000000000000000000008003");
    for (block_output, _) in blocks {
        for diff in &block_output.account_diffs {
            add(diff.address, &mut state_after, &mut seen, &mut preimages);
        }
        // An account whose 0x8003 leaf changed but which is absent from
        // account_diffs (e.g. code force-deployed to an address with zero
        // nonce/balance) still has a tree write the guest must reproduce;
        // its target address is the low 20 bytes of the write's slot key.
        for w in &block_output.storage_writes {
            if w.account == ACCOUNT_PROPERTIES_ADDRESS {
                let addr = Address::from_slice(&w.account_key.0[12..32]);
                add(addr, &mut state_after, &mut seen, &mut preimages);
            }
        }
    }
    Ok(preimages)
}

/// Recover the codes referenced by the after-preimages: every account whose
/// 0x8003 leaf is handed to the guest must have its code available so the
/// guest can recompute the code-derived property fields. Tie code inclusion to
/// preimage inclusion here rather than relying on the input builder's separate
/// (incomplete) upgrade-block bytecode heuristics. Second proof-system only.
fn recover_referenced_bytecodes(
    account_preimages_after: &[(Address, Vec<u8>)],
    read_state: &impl ReadStateHistory,
    blocks: &[BatchBlock<'_>],
) -> anyhow::Result<Vec<(B256, Vec<u8>)>> {
    use zksync_os_interface::traits::PreimageSource;
    let last_block_number = blocks
        .last()
        .context("batch has no blocks")?
        .1
        .block_context
        .block_number;
    let mut state_after = read_state.state_view_at(last_block_number)?;
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(B256, Vec<u8>)> = Vec::new();
    for (addr, preimage) in account_preimages_after {
        let props = zksync_os_zisk_lib::merkle::AccountProperties::decode(preimage)
            .with_context(|| format!("decode the account properties preimage for {addr}"))?;
        let observable = props.observable_bytecode_hash;
        let blake2s = props.bytecode_hash;
        if observable.is_zero() || blake2s.is_zero() || !seen.insert(observable) {
            continue;
        }
        if let Some(blob) = state_after.get_preimage(blake2s) {
            match recover_code_matching(observable, &blob, props.unpadded_code_len as usize) {
                Some(code) => out.push((observable, code)),
                None => tracing::warn!(
                    %addr, %observable,
                    "could not recover code for after-preimage account"
                ),
            }
        }
    }
    Ok(out)
}
