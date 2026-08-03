//! ZiSK batch commitment for the server.
//!
//! This module computes the expected batch public input. It does not verify a
//! proof. The guest commits the value
//! `keccak256(state_before ‖ state_after ‖ chain_config_hash ‖ batch_output_hash)`
//! (`zksync_os_zisk_lib::commitment::batch_public_input_hash`). The proof carries
//! this value in `public_values[32..64]` of the ZiSK v0.18 layout
//! (`programVK (32) ‖ guest publics (256) ‖ vadcop-final VK (32)` = 320 bytes).
//!
//! The server computes the same value here from the batch metadata and binds a
//! submitted proof to it (see `ZiskJobManager::submit_proof`). The computation
//! uses the guest lib's own hash functions, so the two sides cannot drift
//! silently. The full PLONK pairing stays on L1.

use alloy::primitives::B256;
use zisk_witness::ZiskChainConfig;
use zksync_os_contract_interface::models::StoredBatchInfo;
use zksync_os_zisk_lib::commitment;

/// ZiSK public values size: 320 bytes.
///
/// Layout: programVK (32) then the guest publics region (256 bytes: the eight
/// commitment words first, zero-padded after) then the vadcop-final VK (32).
/// The batch commitment occupies bytes [32..64]. The on-chain ZiskVerifier
/// reconstructs its digest from the same 320-byte preimage. The lane's
/// off-chain submission checks validate a submitted payload against this size.
pub const ZISK_PUBLIC_VALUES_BYTES: usize = 320;

/// The batch public input the ZiSK guest commits to, computed from server-side
/// batch metadata with the guest lib's own hash functions.
pub fn expected_zisk_public_input(
    previous_state_commitment: &B256,
    stored_batch_info: &StoredBatchInfo,
    chain_id: u64,
    chain_config: ZiskChainConfig,
) -> B256 {
    let chain_config_hash = commitment::chain_config_hash(
        chain_id,
        chain_config.fri_proof_verification_enabled,
        chain_config.max_tx_gas_limit,
    );
    commitment::batch_public_input_hash(
        previous_state_commitment,
        &stored_batch_info.state_commitment,
        &chain_config_hash,
        &stored_batch_info.commitment,
    )
}
