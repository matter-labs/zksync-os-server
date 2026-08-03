//! Converts server-side block data into a ZiSK `BlockInput` with merkle proofs.
//!
//! Runs a pre-execution pass with REVM to discover all storage reads, then
//! extracts merkle proofs for every accessed slot from the server's merkle tree.

use alloy::consensus::Transaction;
use alloy::eips::Encodable2718;
use alloy::primitives::{Address, B256, Bytes, U256};
use revm::database::CacheDB;
use revm::database_interface::DBErrorMarker;
use revm::primitives::{KECCAK_EMPTY, TxKind};
use revm::state::{AccountInfo, Bytecode};
use revm::{DatabaseRef, ExecuteCommitEvm};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use zksync_os_interface::traits::{PreimageSource, ReadStorage};
use zksync_os_revm::transaction::abstraction::ZKsyncTxBuilder;
use zksync_os_revm::{DefaultZk, ZKsyncTx, ZkBuilder, ZkContext, ZkSpecId};
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord, ViewState};
use zksync_os_types::BlockOutput;
use zksync_os_types::{ExecutionVersion, ZkEnvelope, ZkTransaction};

use versioned_merkle_tree::VersionedMerkleTree;

use serde::{Deserialize, Serialize};
use zksync_os_zisk_lib::merkle::{
    self as zisk_merkle, BatchTreeUpdate, NeighborProofEntry, SlotProofEntry, StorageProof,
    TREE_DEPTH, TreeLeaf, WriteOp,
};
use zksync_os_zisk_lib::types::*;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-block ZiSK data carried through the pipeline.
/// Account preimages and per-slot storage proofs extracted from the tree.
type AccountProofs = (Vec<(Address, Vec<u8>)>, Vec<(B256, StorageProof)>);

/// Read keys, read accounts, storage reads and resolved preimages discovered
/// by the pre-execution pass.
type PreExecutionReads = (
    HashSet<B256>,
    HashSet<Address>,
    Vec<(Address, U256, U256)>,
    Vec<(B256, Vec<u8>)>,
);

mod code_recovery;
mod conversions;
mod pre_execution;
mod state_loading;
mod tree_update;
mod upgrades;
pub use code_recovery::*;
use conversions::*;
use pre_execution::*;
use state_loading::*;
pub use tree_update::*;
use upgrades::*;

/// ERC-1967 implementation storage slot.
/// `bytes32(uint256(keccak256("eip1967.proxy.implementation")) - 1)`
pub(crate) const ERC1967_IMPLEMENTATION_SLOT: &str =
    "360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";

/// ZKsync OS ComplexUpgrader proxy address (system contract at 0x800f).
pub(crate) const COMPLEX_UPGRADER_ADDRESS: &str = "0x000000000000000000000000000000000000800f";

/// Accounts, keccak-keyed bytecodes, and blake2s-keyed full preimages
/// discovered while loading state for the pre-execution pass.
type LoadedAccountsAndBytecodes = (
    HashMap<Address, AccountInfo>,
    HashMap<B256, Bytecode>,
    Vec<(B256, Vec<u8>)>,
);

#[derive(Serialize, Deserialize, Clone)]
pub struct ZiskBlockData {
    pub block_input: BlockInput,
    pub tree_root_before: B256,
    pub leaf_count_before: u64,
    pub block_number_before: u64,
    pub previous_block_timestamp: u64,
    /// All bytecodes needed for this block's execution, keyed by keccak256 hash.
    pub bytecodes: Vec<(B256, Vec<u8>)>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Build per-block ZiSK data from server block data, including merkle proofs.
pub fn build_block_witness<ReadState: ReadStateHistory>(
    block_output: &BlockOutput,
    replay_record: &ReplayRecord,
    mut tree: VersionedMerkleTree,
    native_touched_keys: &[B256],
    read_state: &ReadState,
) -> anyhow::Result<ZiskBlockData> {
    let ctx = &replay_record.block_context;
    let block_number = ctx.block_number;
    // Get leaf_count early (stable across versions), but defer root_hash until
    // after proof extraction to avoid race with concurrent tree updates.
    // The underlying RocksDB is shared via Arc; another thread may apply new
    // blocks between root_info() and merkle_proof() calls.
    let (_initial_root, leaf_count) = tree.root_info()?;
    let basefee: u64 = ctx.eip1559_basefee.try_into().unwrap_or(u64::MAX);
    // Use the block header's prevrandao value from the BlockOutput (computed by Airbender),
    // not the BlockContext's mix_hash (which may be 0). The Airbender VM sets
    // the header's mix_hash to the actual prevrandao value.
    let prev_randao = block_output.header.mix_hash;
    let spec_id = spec_id_from_execution_version(ctx.execution_version)?;

    let transactions = convert_all_txs(&replay_record.transactions, block_output);

    // Phase 1: collect addresses + pre-execute to discover storage reads
    let mut initial_addrs = collect_touched_addresses(replay_record, block_output, ctx.coinbase);

    // For upgrade blocks, include the kernel system contracts the genesis
    // upgrade accesses but that aren't in storage_writes/account_diffs.
    if transactions
        .iter()
        .any(|tx| matches!(tx.auth, TxAuth::Upgrade { .. }))
    {
        // Range 0x10000..=0x1000c, built from bytes (not format!/parse) so a
        // malformed literal can't panic the shared prover-input pipeline.
        const FIRST_KERNEL_SYSTEM_ADDR: u64 = 0x10000;
        const LAST_KERNEL_SYSTEM_ADDR: u64 = 0x1000c;
        for raw in FIRST_KERNEL_SYSTEM_ADDR..=LAST_KERNEL_SYSTEM_ADDR {
            let mut bytes = [0u8; 20];
            bytes[12..20].copy_from_slice(&raw.to_be_bytes());
            let addr = Address::from(bytes);
            if !initial_addrs.contains(&addr) {
                initial_addrs.push(addr);
            }
        }
    }
    let mut state_view = read_state.state_view_at(block_number - 1)?;
    // Create a state view with published preimages as overrides.
    // This resolves system contract bytecodes for genesis/upgrade blocks where
    // preimages are published during execution but aren't in the state view at block N-1.
    let all_preimages: Vec<(B256, Vec<u8>)> = block_output
        .published_preimages
        .iter()
        .chain(&replay_record.force_preimages)
        .cloned()
        .collect();
    let mut state_with_preimages = zksync_os_storage_api::OverriddenStateView::with_preimages(
        state_view.clone(),
        &all_preimages,
    );

    let (mut accounts_map, mut bytecodes_map, mut bytecodes_out) = load_accounts_and_bytecodes(
        &initial_addrs,
        &mut state_with_preimages,
        block_output,
        replay_record,
    );

    // For upgrade txs: pre-create accounts that are force-deployed in this block.
    let has_upgrade = transactions
        .iter()
        .any(|tx| matches!(tx.auth, TxAuth::Upgrade { .. }));
    let mut bytecodes_extra = Vec::new();
    if has_upgrade {
        resolve_upgrade_bytecodes(
            block_output,
            block_number,
            read_state,
            &mut state_view,
            &mut accounts_map,
            &mut bytecodes_map,
            &mut bytecodes_out,
            &mut bytecodes_extra,
        )?;

        // Scan upgrade tx calldata for bytecode hashes and resolve from preimage DB.
        // The inner calldata contains ABI-encoded ForceDeployment structs with bytecodeHash.
        // Scan ALL 32-byte aligned chunks and try each as a preimage key.
        let mut state_for_scan = read_state.state_view_at(block_number)?;
        let mut scanned = HashSet::new();
        let mut calldata_resolved = 0;
        for tx in &transactions {
            let abi_data = match &tx.auth {
                TxAuth::Upgrade { abi_encoded, .. } => abi_encoded,
                _ => continue,
            };
            // Extract calldata from the ABI-encoded L2CanonicalTransaction via
            // a bounds-checked reader: a malformed encoding skips this
            // best-effort scan rather than panicking the shared pipeline task.
            let Some(data) = abi_l2_canonical_calldata(abi_data) else {
                tracing::warn!(
                    block_number,
                    "upgrade tx ABI calldata offset/length out of bounds; \
                     skipping calldata bytecode scan"
                );
                continue;
            };
            // Scan at every byte offset, not just 32-byte aligned,
            // because nested ABI encoding places hashes at arbitrary offsets.
            for offset in 0..data.len() {
                if offset + 32 > data.len() {
                    break;
                }
                let hash = B256::from_slice(&data[offset..offset + 32]);
                if hash.is_zero() || scanned.contains(&hash) || bytecodes_map.contains_key(&hash) {
                    continue;
                }
                scanned.insert(hash);
                if let Some(preimage) = state_for_scan.get_preimage(hash)
                    && preimage.len() > 100
                {
                    {
                        tracing::info!(
                            hash = %hash, preimage_len = preimage.len(),
                            "resolved bytecode from upgrade tx calldata scan"
                        );
                        let bytecode = Bytecode::new_raw(Bytes::copy_from_slice(&preimage));
                        bytecodes_map.insert(hash, bytecode);
                        bytecodes_extra.push((hash, preimage));
                        calldata_resolved += 1;
                    }
                }
            }
        }
        if calldata_resolved > 0 {
            tracing::info!(
                calldata_resolved,
                scanned = scanned.len(),
                "calldata bytecode scan results"
            );
        }
    }

    let mut storage_prestate = load_storage_prestate(&block_output.storage_writes, &mut state_view);

    let mut all_addrs = initial_addrs;
    let mut seen_addrs: HashSet<Address> = all_addrs.iter().copied().collect();
    let mut all_storage_read_keys = HashSet::new();

    // Run pre-execution even for upgrade blocks to discover storage reads.
    // The REVM execution may fail or produce incorrect writes for upgrade txs
    // (since bootloader-level operations aren't reproduced), but the storage
    // reads discovered are needed for merkle proofs.
    let max_iterations = if has_upgrade { 5 } else { 1 };

    for iteration in 0..max_iterations {
        // Always pre-execute against the true pre-state (like the consistency
        // checker): post-state accounts/storage flip the upgrade logic's
        // "already deployed / already initialized" branches and made the
        // upgrade tx revert. Code minted inside this block is bridged via
        // `bytecodes_map` (keccak-keyed), resolved from post-state separately.
        let state_view_for_pre = read_state.state_view_at(block_number - 1)?;
        let (read_keys, extra_addrs, storage_reads, pre_exec_preimages) = pre_execute_for_reads(
            ctx,
            spec_id,
            basefee,
            prev_randao,
            &transactions,
            block_output,
            &accounts_map,
            &storage_prestate,
            &bytecodes_map,
            state_view_for_pre,
        );

        // Merge bytecodes resolved from preimage DB during pre-execution.
        if !pre_exec_preimages.is_empty() {
            tracing::info!(
                count = pre_exec_preimages.len(),
                "pre-execution resolved preimages from DB"
            );
        }
        for (hash, preimage) in pre_exec_preimages {
            bytecodes_map
                .entry(hash)
                .or_insert_with(|| Bytecode::new_raw(Bytes::copy_from_slice(&preimage)));
            bytecodes_extra.push((hash, preimage));
        }

        let new_keys = read_keys.difference(&all_storage_read_keys).count();
        tracing::info!(
            iteration,
            total_read_keys = read_keys.len(),
            new_keys,
            extra_addrs = extra_addrs.len(),
            storage_reads = storage_reads.len(),
            bytecodes_available = bytecodes_map.len(),
            "pre-execution discovery results",
        );
        all_storage_read_keys.extend(read_keys);

        // Merge new storage reads into prestate so next iteration has them
        let mut new_reads = 0;
        for (addr, slot, val) in &storage_reads {
            if storage_prestate.insert((*addr, *slot), *val).is_none() {
                new_reads += 1;
            }
        }

        // Load newly discovered accounts: try pre-execution state first, then post-execution.
        let mut state_view_reload = read_state.state_view_at(block_number - 1)?;
        let mut state_after_reload = if has_upgrade {
            Some(read_state.state_view_at(block_number)?)
        } else {
            None
        };
        let mut new_accounts = 0;
        for addr in extra_addrs {
            if seen_addrs.insert(addr) {
                all_addrs.push(addr);
                new_accounts += 1;
                // Try pre-execution state first
                let mut resolved = false;
                if let Some(props) = state_view_reload.get_account(addr) {
                    let preimage_hash = B256::from(props.bytecode_hash.as_u8_array());
                    let observable_hash = B256::from(props.observable_bytecode_hash.as_u8_array());
                    let effective = if observable_hash.is_zero() {
                        if props.nonce == 0 && props.balance == U256::ZERO {
                            B256::ZERO
                        } else {
                            KECCAK_EMPTY
                        }
                    } else {
                        observable_hash
                    };
                    accounts_map.insert(
                        addr,
                        AccountInfo {
                            nonce: props.nonce,
                            balance: props.balance,
                            code_hash: effective,
                            code: None,
                            account_id: None,
                        },
                    );
                    if !preimage_hash.is_zero()
                        && !observable_hash.is_zero()
                        && let Some(padded_code) = state_view_reload.get_preimage(preimage_hash)
                        && push_code_from_blob(
                            observable_hash,
                            &padded_code,
                            props.unpadded_code_len as usize,
                            &mut bytecodes_map,
                            &mut bytecodes_out,
                        )
                    {
                        resolved = true;
                    }
                }
                // If pre-execution state had observable_hash=0, try post-execution state
                if !resolved
                    && let Some(ref mut state_after) = state_after_reload
                    && let Some(props) = state_after.get_account(addr)
                {
                    let obs_hash = B256::from(props.observable_bytecode_hash.as_u8_array());
                    let pre_hash = B256::from(props.bytecode_hash.as_u8_array());
                    if !obs_hash.is_zero()
                        && !pre_hash.is_zero()
                        && let Some(full_preimage) = state_after.get_preimage(pre_hash)
                    {
                        push_code_from_blob(
                            obs_hash,
                            &full_preimage,
                            props.unpadded_code_len as usize,
                            &mut bytecodes_map,
                            &mut bytecodes_out,
                        );
                        // Deliberately NOT inserted into accounts_map: this
                        // account only exists post-state, and pre-execution
                        // must see the true pre-state (see
                        // resolve_upgrade_bytecodes).
                        // Store full preimage (code+artifacts) under blake2s hash
                        // for deployer precompile.
                        bytecodes_extra.push((pre_hash, full_preimage));
                    }
                }
            }
        }

        tracing::debug!(
            iteration,
            new_keys,
            new_reads,
            new_accounts,
            "Pre-execution iteration"
        );

        // Stable if no new state discovered
        if new_keys == 0 && new_reads == 0 && new_accounts == 0 {
            break;
        }
    }
    // For upgrade txs: add the proxy's implementation slot read to the proof set.
    // The upgrade tx reads the ComplexUpgrader's ERC1967 implementation slot.
    // The ZiSK executor needs a merkle proof for this read.
    if has_upgrade {
        let proxy_addr: Address = COMPLEX_UPGRADER_ADDRESS
            .parse()
            .expect("invalid upgrader address constant");
        let impl_slot = B256::from_slice(
            &alloy::primitives::hex::decode(ERC1967_IMPLEMENTATION_SLOT)
                .expect("invalid ERC1967 slot constant"),
        );
        let flat_key = zisk_merkle::derive_flat_storage_key(&proxy_addr.into_array(), &impl_slot);
        all_storage_read_keys.insert(flat_key);
    }

    let mut state_view = read_state.state_view_at(block_number - 1)?;
    let mut state_view_post = Some(read_state.state_view_at(block_number)?);
    let storage_read_keys = all_storage_read_keys;

    // Phase 2: extract merkle proofs for all accessed keys
    let (account_preimages, mut storage_proofs) =
        extract_account_proofs(&all_addrs, &mut tree, &mut state_view, &mut state_view_post);

    let mut proven_flat_keys: HashSet<B256> = storage_proofs.iter().map(|(k, _)| *k).collect();

    extract_storage_write_proofs(
        &block_output.storage_writes,
        &mut tree,
        &mut proven_flat_keys,
        &mut storage_proofs,
    );

    extract_storage_read_proofs(
        &storage_read_keys,
        &mut tree,
        &mut proven_flat_keys,
        &mut storage_proofs,
    );

    // Include proofs for every key the native execution touched, discovered
    // or not. REVM serves reads on accounts created within the block from its
    // journal (zeros) without consulting the DB, so native-side bookkeeping
    // reads (e.g. initializer reads on freshly force-deployed contracts) are
    // structurally invisible to the pre-execution discovery. The proofs are
    // tree-authenticated, so adding them grants the witness no extra trust.
    let native_keys: HashSet<B256> = native_touched_keys.iter().copied().collect();
    extract_storage_read_proofs(
        &native_keys,
        &mut tree,
        &mut proven_flat_keys,
        &mut storage_proofs,
    );

    // Witness-discovery completeness: every flat key the native execution
    // touched (reads and writes, recorded by the sequencer's tree pass) must
    // have a proof in the witness. The discovery above is heuristic for
    // upgrade blocks (pre-execution passes, preimage scans); this check makes
    // it verified: a gap fails generation loudly here instead of surfacing
    // as a ProvenDB miss at proving time.
    let mut missing = 0usize;
    for key in native_touched_keys {
        if !proven_flat_keys.contains(key) {
            missing += 1;
            tracing::error!(block_number, %key, "witness discovery missed a natively-touched key");
        }
    }
    anyhow::ensure!(
        missing == 0,
        "witness discovery incomplete for block {block_number}: {missing} of {} \
         natively-touched keys have no proof in the witness",
        native_touched_keys.len(),
    );

    // Compute actual tree root from extracted proofs. All proofs were extracted from
    // the same tree version (even if RocksDB was updated concurrently), so they're
    // internally consistent. Use the first proof's root recovery as the authoritative root.
    let root_hash = if let Some((key, proof)) = storage_proofs.first() {
        match proof.verify(key) {
            Ok((root, _)) => root,
            Err(_) => tree.root_info()?.0, // fallback
        }
    } else {
        tree.root_info()?.0 // no proofs, use tree directly
    };

    // Phase 3: assemble
    let block_hashes = extract_block_hashes(&ctx.block_hashes, block_number);

    let l2_to_l1_logs = extract_l2_to_l1_logs(block_output);

    Ok(ZiskBlockData {
        block_input: BlockInput {
            number: block_number,
            timestamp: ctx.timestamp,
            base_fee: basefee,
            gas_limit: ctx.gas_limit,
            coinbase: ctx.coinbase,
            prev_randao,
            // Canonical hash of this block: the guest recomputes the header
            // from its own execution and asserts equality, so any header
            // drift fails loudly at re-execution instead of surfacing as a
            // wrong batch commitment at proving time.
            block_header_hash: block_output.header.hash(),
            storage_proofs,
            account_preimages,
            transactions,
            block_hashes,
            l2_to_l1_logs,
            expected_tree_root: root_hash,
        },
        tree_root_before: root_hash,
        leaf_count_before: leaf_count,
        block_number_before: block_number.saturating_sub(1),
        previous_block_timestamp: replay_record.previous_block_timestamp,
        bytecodes: {
            // All bytecodes keyed by keccak256. Force-deploy preimages are stored
            // in the preimage DB by blake2s; re-key them by keccak256 here.
            let mut all = bytecodes_out;
            let mut seen: std::collections::HashSet<B256> = all.iter().map(|(h, _)| *h).collect();
            for (_blake2s_hash, code) in block_output
                .published_preimages
                .iter()
                .chain(&replay_record.force_preimages)
                .chain(bytecodes_extra.iter())
            {
                let keccak_hash = alloy::primitives::keccak256(code);
                if seen.insert(keccak_hash) {
                    all.push((keccak_hash, code.clone()));
                }
            }
            all
        },
    })
}

/// REVM spec for a ZKsync OS execution version, shared with the consistency
/// checker so both REVM consumers always execute with identical semantics.
/// Errors on unknown versions instead of guessing: a silently wrong spec made
/// the whole second-proof lane diverge from native (upgrade-tx pre-execution
/// reverts, guest receipt/gas drift) when V6 chains ran with `AtlasV2`.
pub fn spec_id_from_execution_version(version: u32) -> anyhow::Result<ZkSpecId> {
    let execution_version = ExecutionVersion::try_from(version)
        .map_err(|e| anyhow::anyhow!("unknown execution version {version}: {e}"))?;
    zksync_os_revm_consistency_checker::helpers::zk_spec_version(execution_version).ok_or_else(
        || anyhow::anyhow!("no REVM spec mapping for execution version {execution_version:?}"),
    )
}

// ---------------------------------------------------------------------------
// Phase 1: Address collection
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tree_update_tests {
    use super::*;
    use alloy::primitives::Address as AlloyAddress;
    use zksync_os_interface::types::StorageWrite;
    use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper, TreeEntry};

    fn write(key: B256, value: B256) -> StorageWrite {
        StorageWrite {
            key,
            value,
            account: AlloyAddress::ZERO,
            account_key: B256::ZERO,
        }
    }

    /// End-to-end: build a real RocksDB tree, produce a witness with
    /// `build_tree_update`, and check that the guest's trust-free
    /// `BatchTreeUpdate::apply` reproduces the tree's actual post-state root.
    /// Mixes updates, inserts far from the touched region (anchor case), and
    /// chained inserts.
    #[test]
    fn guest_apply_matches_real_tree() {
        let dir = tempfile::tempdir().unwrap();
        let db = RocksDBWrapper::new(dir.path()).unwrap();
        let mut tree = MerkleTree::new(db).unwrap();

        // Pre-state: a spread of keys so inserts land between existing leaves.
        let pre: Vec<TreeEntry> = (1u8..=20)
            .map(|i| TreeEntry {
                key: B256::repeat_byte(i * 10),
                value: B256::repeat_byte(i),
            })
            .collect();
        let before = tree.extend(&pre).unwrap();
        let version_before = tree.latest_version().unwrap().unwrap();
        let (root_before, leaf_count_before) = (before.root_hash, before.leaf_count);

        // Post-state: update two existing keys, insert three new ones,
        // including neighbors of untouched regions and adjacent new keys.
        let changes = vec![
            write(B256::repeat_byte(30), B256::repeat_byte(0xa1)), // update
            write(B256::repeat_byte(155), B256::repeat_byte(0xa2)), // insert between 150/160
            write(B256::repeat_byte(156), B256::repeat_byte(0xa3)), // chained insert
            write(B256::repeat_byte(200), B256::repeat_byte(0xa4)), // update
            write(B256::repeat_byte(75), B256::repeat_byte(0xa5)), // insert between 70/80
        ];
        let post_entries: Vec<TreeEntry> = changes
            .iter()
            .map(|w| TreeEntry {
                key: w.key,
                value: w.value,
            })
            .collect();
        let after = tree.extend(&post_entries).unwrap();
        let root_after = after.root_hash;

        // Build the witness from the PRE-state tree view only.
        let mut tree_view = VersionedMerkleTree::new(tree, version_before);
        let update = build_tree_update(&mut tree_view, &changes, leaf_count_before).unwrap();

        // The guest recomputes the post-state root with no trusted input.
        let (computed_root, computed_count) = update.apply(&root_before);
        assert_eq!(
            computed_root, root_after,
            "guest-computed root != real tree root"
        );
        assert_eq!(computed_count, after.leaf_count);
    }
}
