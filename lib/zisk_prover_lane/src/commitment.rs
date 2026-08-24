//! ZiSK batch commitment for the server.
//!
//! This module computes the expected batch public input. It does not verify a
//! proof. Up to and including v31 the guest commits the three-word value
//! `keccak256(state_before ‖ state_after ‖ batch_output_hash)`
//! (`zksync_os_zisk_lib::commitment::batch_public_input_hash`) — the same
//! quantity `Executor._getBatchProofPublicInputZKsyncOS` hashes on the
//! settlement layer. From v32 on a fourth `chain_config_hash` word joins,
//! mirroring the Airbender half's version gate
//! (`node/bin/src/prover_api/fri_proof_verifier.rs`): the two lanes must
//! commit the same value or the settle fails with no off-chain signal. The
//! proof carries this value in `public_values[32..64]` of the ZiSK v0.18
//! layout (`programVK (32) ‖ guest publics (256) ‖ vadcop-final VK (32)` =
//! 320 bytes).
//!
//! The server computes the same value here from the batch metadata and binds a
//! submitted proof to it (see `ZiskJobManager::submit_proof`). The pre-v32
//! computation uses the guest lib's own hash functions, so the two sides
//! cannot drift silently. The full PLONK pairing stays on L1.

use alloy::primitives::{B256, keccak256};
use zksync_os_batch_types::PendingBatchInfo;
use zksync_os_types::ProvingVersion;
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
/// batch metadata and gated on the proving version — during an upgrade window
/// the server verifies batches of two protocol versions at once, so the shape
/// cannot be a compile-time constant.
pub fn expected_zisk_public_input(
    proving_version: ProvingVersion,
    previous_state_commitment: &B256,
    batch_info: &PendingBatchInfo,
    chain_id: u64,
) -> anyhow::Result<B256> {
    match proving_version {
        ProvingVersion::V6 | ProvingVersion::V7 => {
            let stored = batch_info.clone().into_stored();
            Ok(commitment::batch_public_input_hash(
                previous_state_commitment,
                &stored.state_commitment,
                &stored.commitment,
            ))
        }
        // The v0.4.0 ZiSK guest does not exist yet, so there is no guest lib
        // function to defer to; this is the protocol-defined v32 shape, the
        // same one `expected_public_input_registers` verifies for Airbender.
        ProvingVersion::V8 => {
            let chain_config_hash = zksync_os_native_pig::v32_chain_config_hash(chain_id)?;
            Ok(keccak256(
                [
                    previous_state_commitment.0,
                    batch_info.commit_info.new_state_commitment.0,
                    chain_config_hash.0,
                    batch_info.v32_batch_output_hash().0,
                ]
                .concat(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zksync_os_contract_interface::models::{CommitBatchInfo, DACommitmentScheme};
    use zksync_os_types::ProtocolSemanticVersion;

    const TEST_CHAIN_ID: u64 = 270;

    fn batch_info(minor: u64) -> PendingBatchInfo {
        PendingBatchInfo {
            commit_info: CommitBatchInfo {
                batch_number: 7,
                new_state_commitment: B256::repeat_byte(0x22),
                number_of_layer1_txs: 1,
                number_of_layer2_txs: 3,
                priority_operations_hash: B256::repeat_byte(0x33),
                dependency_roots_rolling_hash: B256::repeat_byte(0x44),
                l2_to_l1_logs_root_hash: B256::repeat_byte(0x55),
                l2_da_commitment_scheme: DACommitmentScheme::BlobsAndPubdataKeccak256,
                da_commitment: B256::repeat_byte(0x66),
                first_block_timestamp: 100,
                first_block_number: Some(70),
                last_block_timestamp: 110,
                last_block_number: Some(75),
                chain_id: TEST_CHAIN_ID,
                operator_da_input: vec![],
                sl_chain_id: 1,
            },
            protocol_version: ProtocolSemanticVersion::new(0, minor, 0),
            upgrade_tx_hash: None,
        }
    }

    /// Up to and including v31 the batch public input holds three words:
    /// `keccak(state_before ‖ state_after ‖ batch_output)`. This is what
    /// `Executor._getBatchProofPublicInputZKsyncOS` hashes on the settlement
    /// layer and what zksync-os v0.3.2 `BatchPublicInput::hash` computes, so a
    /// fourth word here fails the settle with no off-chain signal.
    #[test]
    fn v31_batch_public_input_commits_three_words() {
        let previous_state_commitment = B256::repeat_byte(0x11);
        let info = batch_info(31);
        let stored = info.clone().into_stored();

        let proving_version =
            ProvingVersion::try_from(info.protocol_version.clone()).expect("v31 proving version");
        let got = expected_zisk_public_input(
            proving_version,
            &previous_state_commitment,
            &info,
            TEST_CHAIN_ID,
        )
        .expect("three-word public input");

        let want = keccak256(
            [
                previous_state_commitment.0,
                stored.state_commitment.0,
                stored.commitment.0,
            ]
            .concat(),
        );
        assert_eq!(got, want);
    }

    /// From v32 on the batch public input holds four words:
    /// `keccak(state_before ‖ state_after ‖ chain_config_hash ‖ batch_output)`,
    /// with the v32 chain-config commitment and the v32 batch-output layout —
    /// the same quantity the Airbender half verifies for V8.
    #[test]
    fn v32_batch_public_input_commits_four_words() {
        let previous_state_commitment = B256::repeat_byte(0x11);
        let info = batch_info(32);

        let proving_version =
            ProvingVersion::try_from(info.protocol_version.clone()).expect("v32 proving version");
        let got = expected_zisk_public_input(
            proving_version,
            &previous_state_commitment,
            &info,
            TEST_CHAIN_ID,
        )
        .expect("four-word public input");

        let chain_config_hash = zksync_os_native_pig::v32_chain_config_hash(TEST_CHAIN_ID)
            .expect("v32 chain config hash");
        let want = keccak256(
            [
                previous_state_commitment.0,
                info.commit_info.new_state_commitment.0,
                chain_config_hash.0,
                info.v32_batch_output_hash().0,
            ]
            .concat(),
        );
        assert_eq!(got, want);
    }
}
