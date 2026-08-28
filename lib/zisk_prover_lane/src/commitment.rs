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
//! proof carries this value in the guest publics region of the ZiSK
//! v1.2.0-alpha layout (`programVK (32) ‖ guest publics (512) ‖
//! vadcop-final VK (32)` = 576 bytes).
//!
//! The server computes the same value here from the batch metadata and binds a
//! submitted proof to it (see `ZiskJobManager::submit_proof`). The pre-v32
//! computation uses the guest lib's own hash functions, so the two sides
//! cannot drift silently. The full PLONK pairing stays on L1.

use alloy::primitives::{B256, keccak256};
use zksync_os_batch_types::PendingBatchInfo;
use zksync_os_types::ProvingVersion;
use zksync_os_zisk_lib::commitment;

/// ZiSK public values size: 576 bytes.
///
/// Layout: programVK (32) then the guest publics region (512 bytes: ziskos's
/// 64-word output area, eight little-endian bytes per word, the eight
/// commitment words first and zeros after) then the vadcop-final VK (32).
/// The on-chain ZiskVerifier reconstructs its digest from the same 576-byte
/// preimage. The lane's off-chain submission checks validate a submitted
/// payload against this size.
pub const ZISK_PUBLIC_VALUES_BYTES: usize = 576;

/// Byte range of the value a guest commits, inside the wire public values.
///
/// A guest commits eight u32 words through `ziskos::io::commit_slice`, and
/// each word reaches the wire widened to a little-endian u64. The 32-byte
/// value therefore spans 64 bytes, four significant bytes per eight-byte
/// slot.
pub const ZISK_COMMITTED_VALUE_RANGE: std::ops::Range<usize> = 32..96;

/// The 32 bytes a ZiSK guest committed, read from the wire public values:
/// the batch commitment on a per-batch proof, the binding digest on an
/// aggregated range proof.
///
/// The high four bytes of every slot carry no payload and are ignored, which
/// matches how the guest reads back its own publics.
/// `None` when the public values are too short to hold the region.
pub fn committed_value(public_values: &[u8]) -> Option<B256> {
    let region = public_values.get(ZISK_COMMITTED_VALUE_RANGE)?;
    let mut out = [0u8; 32];
    for (slot, chunk) in region.chunks_exact(8).zip(out.chunks_exact_mut(4)) {
        chunk.copy_from_slice(&slot[..4]);
    }
    Some(B256::from(out))
}

/// Wire public values that carry `value` where a guest's commitment sits, and
/// zeros elsewhere. Exposed for tests in other crates via the `test-support`
/// feature; the VK fields stay zero, so a caller that needs a VK tripwire to
/// fire must fill them itself.
#[cfg(any(test, feature = "test-support"))]
pub fn synthetic_public_values(value: B256) -> Vec<u8> {
    let mut public_values = vec![0u8; ZISK_PUBLIC_VALUES_BYTES];
    let region = &mut public_values[ZISK_COMMITTED_VALUE_RANGE];
    for (slot, chunk) in region
        .chunks_exact_mut(8)
        .zip(value.as_slice().chunks_exact(4))
    {
        slot[..4].copy_from_slice(chunk);
    }
    public_values
}

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

    /// The committed value is spread four bytes per eight-byte slot, so a
    /// reader that took a flat 32-byte window would see a different value.
    #[test]
    fn committed_value_reads_four_bytes_per_word() {
        let value = B256::from([
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D,
            0x0E, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B,
            0x1C, 0x1D, 0x1E, 0x1F,
        ]);
        let public_values = synthetic_public_values(value);

        assert_eq!(public_values.len(), ZISK_PUBLIC_VALUES_BYTES);
        assert_eq!(
            &public_values[32..40],
            &[0x00, 0x01, 0x02, 0x03, 0, 0, 0, 0]
        );
        assert_eq!(
            &public_values[88..96],
            &[0x1C, 0x1D, 0x1E, 0x1F, 0, 0, 0, 0]
        );
        assert_eq!(committed_value(&public_values), Some(value));
    }

    /// The high half of every slot carries no payload, so a prover that fills
    /// it cannot change the value the server binds.
    #[test]
    fn committed_value_ignores_the_high_half_of_each_word() {
        let value = B256::repeat_byte(0x11);
        let mut public_values = synthetic_public_values(value);
        for slot in public_values[ZISK_COMMITTED_VALUE_RANGE].chunks_exact_mut(8) {
            slot[4..].copy_from_slice(&[0xDE; 4]);
        }
        assert_eq!(committed_value(&public_values), Some(value));
    }

    #[test]
    fn committed_value_rejects_short_public_values() {
        assert_eq!(committed_value(&[0u8; 95]), None);
    }

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
