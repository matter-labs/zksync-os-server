//! Decode semantics of the consensus block envelope: identity follows the received
//! bytes, and a frame carries exactly one record.
#![cfg(feature = "consensus")]

use alloy::primitives::{Address, B256, U256};
use commonware_codec::{EncodeSize, Read as _, Write};
use commonware_cryptography::Digestible;
use zksync_os_storage_api::{BlockContext, BlockHashes, ReplayRecord};
use zksync_os_types::{BlockStartCursors, ExecutionVersion, ProtocolSemanticVersion};
use zksync_os_wire::ConsensusBlock;

/// A minimal valid record for block 1 (no transactions — transaction encodings are
/// the golden suite's business; these tests are about the envelope).
fn empty_record() -> ReplayRecord {
    let protocol_version: ProtocolSemanticVersion = "0.31.0".parse().expect("valid version");
    let execution_version: ExecutionVersion =
        (&protocol_version).try_into().expect("supported version");
    ReplayRecord {
        block_context: BlockContext {
            chain_id: 270,
            block_number: 1,
            block_hashes: BlockHashes::default(),
            timestamp: 1_700_000_001,
            eip1559_basefee: U256::from(1_000),
            pubdata_price: U256::from(100),
            native_price: U256::from(1_000),
            coinbase: Address::repeat_byte(9),
            gas_limit: 100_000_000,
            pubdata_limit: 110_000,
            mix_hash: U256::ZERO,
            execution_version: execution_version as u32,
            blob_fee: U256::ONE,
        },
        transactions: Vec::new(),
        previous_block_timestamp: 1_700_000_000,
        node_version: semver::Version::new(0, 0, 0),
        protocol_version,
        block_output_hash: B256::repeat_byte(0xBB),
        force_preimages: Vec::new(),
        starting_cursors: BlockStartCursors::default(),
    }
}

fn encode_block(block: &ConsensusBlock) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(block.encode_size());
    block.write(&mut encoded);
    encoded
}

/// Decoding must reproduce the identical identity AND the identical bytes: the
/// received encoding — not a local re-encoding — is what the digest hashes and what
/// any re-serialization (storage, forwarding) emits. This is the property that lets
/// the committee ever speak more than one record wire version.
#[test]
fn digest_and_reserialization_follow_the_received_bytes() {
    let genesis = ConsensusBlock::genesis(B256::repeat_byte(0x11));
    let block = ConsensusBlock::from_record(&genesis, empty_record());
    let encoded = encode_block(&block);

    let decoded =
        ConsensusBlock::read_cfg(&mut encoded.as_slice(), &0).expect("valid block decodes");
    assert_eq!(decoded.digest(), block.digest());
    assert_eq!(
        encode_block(&decoded),
        encoded,
        "re-serialization must emit exactly the received bytes",
    );
}

/// The frame's length prefix must cover exactly one record. Trailing bytes would let
/// two different wire forms carry the same logical block — rejected outright.
#[test]
fn record_frames_with_trailing_garbage_are_rejected() {
    let genesis = ConsensusBlock::genesis(B256::repeat_byte(0x11));
    let block = ConsensusBlock::from_record(&genesis, empty_record());
    let mut encoded = encode_block(&block);

    // Frame layout: height (8) | parent digest (32) | record flag (1) | record
    // length (8) | record bytes. Grow the declared length by one and append one
    // garbage byte — still a structurally well-formed frame.
    const LENGTH_PREFIX_AT: usize = 8 + 32 + 1;
    let length_bytes: [u8; 8] = encoded[LENGTH_PREFIX_AT..LENGTH_PREFIX_AT + 8]
        .try_into()
        .expect("eight bytes");
    let padded_length = u64::from_be_bytes(length_bytes) + 1;
    encoded[LENGTH_PREFIX_AT..LENGTH_PREFIX_AT + 8].copy_from_slice(&padded_length.to_be_bytes());
    encoded.push(0x00);

    assert!(
        ConsensusBlock::read_cfg(&mut encoded.as_slice(), &0).is_err(),
        "a record frame with trailing bytes must not decode",
    );
}

/// A record framed with a future version discriminator (a first byte below RLP's
/// list-prefix range) is a leader speaking a wire version this node does not know:
/// a clean, named decode error — the routine no-vote path — never a panic and never
/// misinterpretation as v3.
#[test]
fn future_record_versions_are_rejected_cleanly() {
    let genesis = ConsensusBlock::genesis(B256::repeat_byte(0x11));
    let block = ConsensusBlock::from_record(&genesis, empty_record());
    let mut encoded = encode_block(&block);

    // Replace the record bytes with a hypothetical v4 frame: discriminator 0x04
    // followed by arbitrary payload. Adjust the length prefix accordingly.
    const LENGTH_PREFIX_AT: usize = 8 + 32 + 1;
    let fake_record = [0x04, 0xDE, 0xAD, 0xBE, 0xEF];
    encoded.truncate(LENGTH_PREFIX_AT);
    encoded.extend_from_slice(&(fake_record.len() as u64).to_be_bytes());
    encoded.extend_from_slice(&fake_record);

    let result = ConsensusBlock::read_cfg(&mut encoded.as_slice(), &0);
    assert!(
        result.is_err(),
        "an unknown record wire version must not decode",
    );
}
