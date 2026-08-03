//! Loading touched accounts, bytecodes and storage pre-state, and extracting merkle proofs for accessed slots.

use super::*;

pub(super) fn collect_touched_addresses(
    replay_record: &ReplayRecord,
    block_output: &BlockOutput,
    coinbase: Address,
) -> Vec<Address> {
    let mut addrs = Vec::new();
    let mut seen = HashSet::new();
    let push = |addr: Address, seen: &mut HashSet<Address>, addrs: &mut Vec<Address>| {
        if seen.insert(addr) {
            addrs.push(addr);
        }
    };
    for tx in &replay_record.transactions {
        push(tx.signer(), &mut seen, &mut addrs);
        if let Some(to) = tx.to() {
            push(to, &mut seen, &mut addrs);
        }
    }
    push(coinbase, &mut seen, &mut addrs);
    // REVM calls basic_ref on precompile addresses during CALL (for balance checks),
    // even though the precompile intercepts execution later. Include them explicitly.
    for sys in [0x8006u64, 0x8008, 0x800a] {
        let mut bytes = [0u8; 20];
        bytes[18..].copy_from_slice(&(sys as u16).to_be_bytes());
        push(Address::from(bytes), &mut seen, &mut addrs);
    }
    for diff in &block_output.account_diffs {
        push(diff.address, &mut seen, &mut addrs);
    }
    // Include all addresses that have storage writes; these are system contracts
    // (ContractDeployer, NonceHolder, AccountCodeStorage, etc.) that the upgrade
    // transaction modifies. REVM needs their bytecodes to execute the call chain.
    for write in &block_output.storage_writes {
        push(write.account, &mut seen, &mut addrs);
    }
    addrs
}

pub(super) fn load_accounts_and_bytecodes<S: ViewState>(
    addrs: &[Address],
    state_view: &mut S,
    block_output: &BlockOutput,
    replay_record: &ReplayRecord,
) -> LoadedAccountsAndBytecodes {
    let mut accounts = HashMap::new();
    let mut bytecodes_map = HashMap::new();
    let mut bytecodes_out = Vec::new();
    let mut seen_hashes = HashSet::new();

    // Build a lookup for force_preimages (system contract bytecodes from upgrade tx)
    let force_preimage_map: HashMap<B256, &Vec<u8>> = block_output
        .published_preimages
        .iter()
        .chain(&replay_record.force_preimages)
        .map(|(h, c)| (*h, c))
        .collect();
    if !force_preimage_map.is_empty() {
        tracing::debug!(
            count = force_preimage_map.len(),
            "Force preimages available for bytecode resolution"
        );
    }

    for &addr in addrs {
        if let Some(props) = state_view.get_account(addr) {
            // AccountProperties has two hashes:
            // - bytecode_hash (blake2s256 of padded code): used for preimage DB lookup
            // - observable_bytecode_hash (keccak256 of raw code): the EVM-visible code_hash
            // REVM uses keccak256 as code_hash, so we use observable_bytecode_hash.
            let observable_hash = B256::from(props.observable_bytecode_hash.as_u8_array());
            let preimage_hash = B256::from(props.bytecode_hash.as_u8_array());

            let mut effective = if observable_hash.is_zero() {
                if props.nonce == 0 && props.balance == U256::ZERO {
                    B256::ZERO
                } else {
                    KECCAK_EMPTY
                }
            } else {
                observable_hash // keccak256 of raw code, correct for REVM
            };
            // Load bytecode: preimage DB stores (blake2s256(padded), padded_code).
            // REVM needs (keccak256(raw), raw_code). Extract raw code by truncating
            // padding using unpadded_code_len from AccountProperties.
            if !preimage_hash.is_zero() {
                if !observable_hash.is_zero() {
                    if let Some(padded_code) = state_view.get_preimage(preimage_hash)
                        && seen_hashes.insert(preimage_hash)
                    {
                        push_code_from_blob(
                            observable_hash,
                            &padded_code,
                            props.unpadded_code_len as usize,
                            &mut bytecodes_map,
                            &mut bytecodes_out,
                        );
                    }
                } else if let Some(code) = force_preimage_map.get(&preimage_hash) {
                    // Account has bytecode_hash (blake2s) but no observable_bytecode_hash (keccak).
                    // This happens for system contracts during genesis/upgrade: they're deployed via
                    // force_preimages but haven't had their observable hash set yet.
                    // Resolve the bytecode from force_preimages and compute keccak256 as code_hash.
                    let keccak_hash = alloy::primitives::keccak256(code);
                    effective = keccak_hash;
                    if seen_hashes.insert(preimage_hash) {
                        bytecodes_out.push((keccak_hash, code.to_vec()));
                        bytecodes_map
                            .insert(keccak_hash, Bytecode::new_raw(Bytes::copy_from_slice(code)));
                    }
                    tracing::debug!(
                        addr = %addr,
                        blake2s_hash = %preimage_hash,
                        keccak_hash = %keccak_hash,
                        code_len = code.len(),
                        "resolved force_preimage bytecode for account with zero observable_hash"
                    );
                }
            }
            accounts.insert(
                addr,
                AccountInfo {
                    nonce: props.nonce,
                    balance: props.balance,
                    code_hash: effective,
                    code: None,
                    account_id: None,
                },
            );
        }
    }
    // Load force_preimages (system contract bytecodes from upgrade/genesis) into bytecodes.
    for (hash, code) in block_output
        .published_preimages
        .iter()
        .chain(&replay_record.force_preimages)
    {
        let keccak_hash = alloy::primitives::keccak256(code);
        if seen_hashes.insert(*hash) {
            bytecodes_out.push((keccak_hash, code.clone()));
            let bytecode = Bytecode::new_raw(Bytes::copy_from_slice(code));
            bytecodes_map.insert(keccak_hash, bytecode.clone());
            // Also store under original blake2s hash for deployer precompile / pre-execution.
            bytecodes_map.insert(*hash, bytecode);
        }
    }
    (accounts, bytecodes_map, bytecodes_out)
}

pub(super) fn load_storage_prestate(
    writes: &[zksync_os_interface::types::StorageWrite],
    state_view: &mut impl ReadStorage,
) -> HashMap<(Address, U256), U256> {
    let mut prestate = HashMap::new();
    for w in writes {
        let old = state_view.read(w.key).unwrap_or(B256::ZERO);
        prestate.insert(
            (w.account, U256::from_be_bytes(w.account_key.0)),
            U256::from_be_bytes(old.0),
        );
    }
    prestate
}

// ---------------------------------------------------------------------------
// Phase 1: Pre-execution for read tracking
// ---------------------------------------------------------------------------

/// Returns (storage_read_flat_keys, extra_account_addresses, storage_reads).
/// storage_reads: (address, slot, value) for all storage accessed during pre-execution.
pub(super) fn extract_account_proofs<S1: ViewState, S2: ViewState>(
    addrs: &[Address],
    tree: &mut VersionedMerkleTree,
    state_view: &mut S1,
    state_view_post: &mut Option<S2>,
) -> AccountProofs {
    let mut preimages = Vec::new();
    let mut proofs = Vec::new();

    for &addr in addrs {
        let flat_key = account_flat_key(addr);
        let proof = extract_proof(tree, flat_key);
        proofs.push((flat_key, proof));

        if let Some(hash_value) = ReadStorage::read(state_view, flat_key) {
            // Try pre-execution state first, fall back to post-execution state.
            // Some system contracts have preimages only in the post-execution state
            // (e.g. force-deployed contracts in genesis/upgrade blocks).
            let preimage = state_view.get_preimage(hash_value).or_else(|| {
                state_view_post
                    .as_mut()
                    .and_then(|sv| sv.get_preimage(hash_value))
            });
            if let Some(preimage) = preimage {
                preimages.push((addr, preimage));
            } else {
                tracing::warn!(
                    addr = %addr, hash = %hash_value,
                    "account exists in tree but no preimage found in pre or post-execution state"
                );
            }
        }
    }
    (preimages, proofs)
}

pub(super) fn extract_storage_write_proofs(
    writes: &[zksync_os_interface::types::StorageWrite],
    tree: &mut VersionedMerkleTree,
    proven: &mut HashSet<B256>,
    proofs: &mut Vec<(B256, StorageProof)>,
) {
    for w in writes {
        if proven.insert(w.key) {
            proofs.push((w.key, extract_proof(tree, w.key)));
        }
    }
}

pub(super) fn extract_storage_read_proofs(
    read_keys: &HashSet<B256>,
    tree: &mut VersionedMerkleTree,
    proven: &mut HashSet<B256>,
    proofs: &mut Vec<(B256, StorageProof)>,
) {
    for &key in read_keys {
        if proven.insert(key) {
            proofs.push((key, extract_proof(tree, key)));
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 3: Tree update proof construction
// ---------------------------------------------------------------------------
