//! ZiSK shadow-execution self-check (`zisk_shadow_execution`).
//!
//! Re-execute a sealed batch's assembled `BatchInput` in-process with the guest
//! executor and compare the computed batch public input against the expected
//! one. The full guest pipeline — witness, ProvenDB, tree update, header
//! commitments, PI — runs per batch without proving. A mismatch is the headline
//! divergence signal; under `halt_on_shadow_mismatch` it fails batch sealing
//! loudly. The batcher seal path in `node/bin` calls this.

use crate::commitment::expected_zisk_public_input;
use crate::metrics::ZISK_LANE_METRICS;
use alloy::primitives::B256;
use zisk_witness::ZiskChainConfig;
use zksync_os_batch_types::PendingBatchInfo;
use zksync_os_batch_types::batcher_model::ProverInput;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::BlockOutput;

/// Re-execute the batch's assembled `BatchInput` and compare the computed batch
/// public input against the expected one. Returns an error only under
/// `halt_on_shadow_mismatch`; otherwise a divergence is logged and counted and
/// `Ok` is returned so batch sealing proceeds.
pub fn shadow_execute_zisk_batch(
    zisk_data: &[u8],
    previous_state_commitment: &B256,
    batch_info: &PendingBatchInfo,
    chain_id: u64,
    zisk_chain_config: ZiskChainConfig,
    halt_on_shadow_mismatch: bool,
    blocks: &[(
        BlockOutput,
        ReplayRecord,
        zksync_os_merkle_tree::TreeBatchOutput,
        ProverInput,
    )],
) -> anyhow::Result<()> {
    let batch_number = batch_info.commit_info.batch_number;
    let stored = batch_info.clone().into_stored();
    let expected = expected_zisk_public_input(
        previous_state_commitment,
        &stored,
        chain_id,
        zisk_chain_config,
    );

    let started = std::time::Instant::now();
    // The guest executor asserts internally (header hashes, tree roots, log
    // consistency); a panic is a divergence report, not a node crash.
    let result = std::panic::catch_unwind(|| {
        zksync_os_zisk_lib::executor::execute_and_commit_from_bincode(zisk_data)
    });
    let elapsed = started.elapsed();
    ZISK_LANE_METRICS.shadow_execution_time.observe(elapsed);

    let failure = match result {
        Ok(Ok((_, commitment))) if commitment == expected => {
            tracing::info!(
                batch_number,
                %commitment,
                elapsed_ms = elapsed.as_millis() as u64,
                "ZiSK shadow execution matched the expected batch public input"
            );
            return Ok(());
        }
        Ok(Ok((_, commitment))) => {
            // Component-level diagnostics: re-run the debug variant to see
            // which PI word drifted (state commitments, chain config, batch
            // output hash).
            if let Ok(input) =
                zksync_os_zisk_lib::wire::decode::<zksync_os_zisk_lib::types::BatchInput>(zisk_data)
            {
                let (_, _, g_before, g_after, g_batch) =
                    zksync_os_zisk_lib::executor::execute_and_commit_debug(&input);
                let chain_config_hash = zksync_os_zisk_lib::commitment::chain_config_hash(
                    chain_id,
                    zisk_chain_config.fri_proof_verification_enabled,
                    zisk_chain_config.max_tx_gas_limit,
                );
                tracing::error!(
                    batch_number,
                    guest_state_before = %g_before,
                    server_state_before = %previous_state_commitment,
                    guest_state_after = %g_after,
                    server_state_after = %stored.state_commitment,
                    guest_batch_output_hash = %g_batch,
                    server_batch_commitment = %stored.commitment,
                    %chain_config_hash,
                    "ZiSK shadow execution PI components"
                );
            }
            format!("guest computed {commitment}, expected {expected}")
        }
        Ok(Err(e)) => format!("guest execution failed: {e}"),
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            format!("guest execution panicked: {msg}")
        }
    };

    // Name any flat keys mentioned in the failure: map them back to the
    // batch's native writes so divergences arrive as (address, slot), not
    // opaque hashes.
    for hex_key in failure
        .split(|c: char| !c.is_ascii_hexdigit() && c != 'x')
        .filter(|w| w.len() == 66 && w.starts_with("0x"))
    {
        if let Ok(flat_key) = hex_key.parse::<B256>() {
            for (block_output, _, _, _) in blocks {
                for w in &block_output.storage_writes {
                    if w.key == flat_key {
                        tracing::error!(
                            batch_number, %flat_key, account = %w.account,
                            slot = %w.account_key, value = %w.value,
                            "divergent key is a native storage write"
                        );
                    }
                }
                for d in &block_output.account_diffs {
                    let props_key = zisk_witness::account_flat_key(d.address);
                    if props_key == flat_key {
                        tracing::error!(
                            batch_number, %flat_key, account = %d.address,
                            nonce = d.nonce, balance = %d.balance,
                            "divergent key is a native account-properties write"
                        );
                    }
                }
            }
        }
    }

    ZISK_LANE_METRICS.commitment_mismatches.inc();
    tracing::error!(batch_number, "ZiSK shadow execution divergence: {failure}");
    if halt_on_shadow_mismatch {
        anyhow::bail!("ZiSK shadow execution divergence on batch {batch_number}: {failure}");
    }
    Ok(())
}
