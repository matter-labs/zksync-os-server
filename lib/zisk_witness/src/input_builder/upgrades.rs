//! Resolving force-deployed bytecodes for protocol upgrade transactions.

use super::{
    AccountInfo, Address, B256, BlockOutput, Bytecode, Bytes, COMPLEX_UPGRADER_ADDRESS,
    ERC1967_IMPLEMENTATION_SLOT, HashMap, HashSet, KECCAK_EMPTY, ReadStateHistory, ReadStorage,
    U256, push_code_from_blob, zisk_merkle,
};
use zksync_os_interface::traits::PreimageSource;
use zksync_os_storage_api::ViewState;
/// Resolve the bytecodes minted by an upgrade block's force deployments so
/// the pre-execution and the guest can look them up by their keccak256 hash.
///
/// Accounts are deliberately NOT materialized from post-state: the upgrade
/// logic branches on whether its targets already exist (deploy/initialize vs
/// skip), so pre-execution must see the true pre-state — mirroring the
/// consistency checker, which only preloads a keccak-keyed code cache.
#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_upgrade_bytecodes<ReadState: ReadStateHistory>(
    block_output: &BlockOutput,
    block_number: u64,
    read_state: &ReadState,
    state_view: &mut impl ReadStorage,
    accounts_map: &mut HashMap<Address, AccountInfo>,
    bytecodes_map: &mut HashMap<B256, Bytecode>,
    bytecodes_out: &mut Vec<(B256, Vec<u8>)>,
    extra_bytecodes: &mut Vec<(B256, Vec<u8>)>,
) -> anyhow::Result<()> {
    // Resolve bytecodes minted by the force deployments from post-execution
    // state, WITHOUT materializing the accounts: pre-execution must see the
    // true pre-state (a force-deploy target that already "exists" flips the
    // upgrade logic's deployed/initialized branches — the checker proved the
    // upgrade executes correctly against pristine pre-state plus a
    // keccak-keyed code cache).
    {
        let mut state_after = read_state.state_view_at(block_number)?;
        let addrs_needing_code: Vec<Address> = block_output
            .storage_writes
            .iter()
            .map(|w| w.account)
            .chain(block_output.account_diffs.iter().map(|d| d.address))
            .filter(|addr| {
                accounts_map.get(addr).is_none_or(|info| {
                    info.code_hash == KECCAK_EMPTY || info.code_hash == B256::ZERO
                })
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        tracing::info!(
            count = addrs_needing_code.len(),
            "resolving post-execution bytecodes"
        );
        for addr in addrs_needing_code {
            if let Some(props) = state_after.get_account(addr) {
                let obs_hash = B256::from(props.observable_bytecode_hash.as_u8_array());
                let pre_hash = B256::from(props.bytecode_hash.as_u8_array());
                if obs_hash.is_zero() {
                    continue;
                }
                let effective = obs_hash;
                if !pre_hash.is_zero()
                    && let Some(padded_code_with_artifacts) = state_after.get_preimage(pre_hash)
                {
                    {
                        push_code_from_blob(
                            effective,
                            &padded_code_with_artifacts,
                            props.unpadded_code_len as usize,
                            bytecodes_map,
                            bytecodes_out,
                        );
                        // Store the FULL preimage (code + artifacts) under the blake2s hash.
                        // The deployer precompile looks up by this hash.
                        tracing::debug!(
                            addr = %addr,
                            blake2s_hash = %pre_hash,
                            preimage_len = padded_code_with_artifacts.len(),
                            "storing force_deploy_bytecode (blake2s → preimage)"
                        );
                        extra_bytecodes.push((pre_hash, padded_code_with_artifacts));
                    }
                }
                tracing::info!(
                    addr = %addr, code_hash = %effective, code_len = bytecodes_map.get(&effective).map(|b| b.len()).unwrap_or(0),
                    "resolved post-execution bytecode for force-deployed account"
                );
            }
        }
    }

    // Resolve deployer bytecode hashes captured during server execution.
    // The server marks these with 1-byte marker values in published_preimages.
    {
        let mut state_for_deployer = read_state.state_view_at(block_number)?;
        let mut deployer_resolved = 0;
        for (hash, marker) in &block_output.published_preimages {
            // 0xDE marker: deployer bytecode hash whose preimage must be resolved.
            if marker.len() == 1
                && marker[0] == 0xDE
                && let Some(preimage) = state_for_deployer.get_preimage(*hash)
                && !preimage.is_empty()
            {
                bytecodes_map
                    .entry(*hash)
                    .or_insert_with(|| Bytecode::new_raw(Bytes::copy_from_slice(&preimage)));
                extra_bytecodes.push((*hash, preimage));
                deployer_resolved += 1;
            }
        }
        if deployer_resolved > 0 {
            tracing::info!(deployer_resolved, "resolved deployer bytecode preimages");
        }
    }

    // Include ALL published_preimages as extra_bytecodes.
    // Published preimages include both AccountProperties (short, ~124 bytes) and
    // actual bytecodes (long, thousands of bytes). The deployer precompile's
    // setDeployedCodeEVM uses the blake2s hash to look up bytecodes.
    // By including all published preimages, the deployer can find them.
    {
        let mut extra_from_published = 0;
        for (hash, data) in &block_output.published_preimages {
            if !data.is_empty() && !bytecodes_map.contains_key(hash) {
                bytecodes_map.insert(*hash, Bytecode::new_raw(Bytes::copy_from_slice(data)));
                extra_bytecodes.push((*hash, data.clone()));
                extra_from_published += 1;
            }
        }
        if extra_from_published > 0 {
            tracing::info!(
                extra_from_published,
                "included published_preimages as extra_bytecodes"
            );
        }
    }

    // Extract bytecode preimages from storage writes to 0x8003 (AccountProperties).
    // Each write stores a hash of AccountProperties. Resolve the preimage, decode the
    // bytecode_hash field, then resolve the bytecode preimage from the DB.
    // This captures ALL bytecodes deployed during the upgrade, including those the
    // deployer precompile installs via setDeployedCodeEVM.
    {
        let mut state_for_bytecodes = read_state.state_view_at(block_number)?;
        let mut seen_bytecode_hashes = HashSet::new();
        let mut resolved_count = 0;
        // Scan ALL storage write values as potential preimage keys.
        // AccountProperties writes have account-property-hash values.
        // Other writes may reference bytecode hashes.
        let mut total_writes = 0;
        let mut preimage_found = 0;
        let mut props_decoded = 0;
        for write in &block_output.storage_writes {
            let value_hash = write.value;
            if value_hash.is_zero() {
                continue;
            }
            total_writes += 1;
            // Resolve AccountProperties preimage
            if let Some(props_bytes) = state_for_bytecodes.get_preimage(value_hash) {
                preimage_found += 1;
                // The scan tries EVERY storage-write value as a preimage key, so
                // most hits here are NOT AccountProperties (they are full
                // bytecodes, thousands of bytes). The decoder rejects those on
                // length, and the scan moves to the next write.
                // AccountProperties layout: versioning_data(4) + nonce(8) + balance(32) + bytecode_hash(32) + ...
                let Ok(props) = zisk_merkle::AccountProperties::decode(&props_bytes) else {
                    continue;
                };
                props_decoded += 1;
                let bytecode_hash = props.bytecode_hash;
                if !bytecode_hash.is_zero() {
                    tracing::debug!(
                        account = %write.account,
                        bytecode_hash = %bytecode_hash,
                        "AccountProperties scan: found bytecode_hash"
                    );
                }
                if bytecode_hash.is_zero() || !seen_bytecode_hashes.insert(bytecode_hash) {
                    continue;
                }
                if bytecodes_map.contains_key(&bytecode_hash) {
                    continue;
                }
                if let Some(code_preimage) = state_for_bytecodes.get_preimage(bytecode_hash)
                    && !code_preimage.is_empty()
                {
                    bytecodes_map.insert(
                        bytecode_hash,
                        Bytecode::new_raw(Bytes::copy_from_slice(&code_preimage)),
                    );
                    extra_bytecodes.push((bytecode_hash, code_preimage));
                    resolved_count += 1;
                }
            }
        }
        tracing::info!(
            total_writes,
            preimage_found,
            props_decoded,
            resolved_count,
            "AccountProperties bytecode scan stats"
        );
        if resolved_count > 0 {
            tracing::info!(
                resolved_count,
                "extracted bytecode preimages from AccountProperties storage writes"
            );
        }
    }

    // Resolve the proxy implementation target via the ERC1967 storage slot.
    let proxy_addr: Address = COMPLEX_UPGRADER_ADDRESS
        .parse()
        .expect("invalid upgrader address constant");
    let impl_slot = U256::from_be_bytes(
        B256::from_slice(
            &alloy::primitives::hex::decode(ERC1967_IMPLEMENTATION_SLOT)
                .expect("invalid ERC1967 slot constant"),
        )
        .0,
    );
    let impl_flat_key = zisk_merkle::derive_flat_storage_key(
        &proxy_addr.into_array(),
        &B256::from(impl_slot.to_be_bytes::<32>()),
    );

    let Some(impl_value) = state_view.read(impl_flat_key) else {
        return Ok(());
    };
    let impl_addr = Address::from_slice(&impl_value.0[12..32]);
    if impl_addr.is_zero() || accounts_map.contains_key(&impl_addr) {
        return Ok(());
    }

    // Load the implementation from post-execution state (it's force-deployed in this block).
    let mut state_after = read_state.state_view_at(block_number)?;
    if let Some(props) = state_after.get_account(impl_addr) {
        let obs_hash = B256::from(props.observable_bytecode_hash.as_u8_array());
        let pre_hash = B256::from(props.bytecode_hash.as_u8_array());
        let effective = if obs_hash.is_zero() {
            KECCAK_EMPTY
        } else {
            obs_hash
        };
        tracing::info!(address = %impl_addr, code_hash = %effective, "resolving upgrade implementation bytecode");
        if !pre_hash.is_zero()
            && let Some(padded_code) = state_after.get_preimage(pre_hash)
        {
            {
                push_code_from_blob(
                    effective,
                    &padded_code,
                    props.unpadded_code_len as usize,
                    bytecodes_map,
                    bytecodes_out,
                );
            }
        }
    }
    Ok(())
}
