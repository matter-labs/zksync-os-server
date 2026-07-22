//! Adversarial decoding: the golden corpus, damaged on purpose.
//!
//! The goldens pin that valid bytes stay valid; these tests pin what happens to
//! *invalid* bytes. Every decoder here parses peer-supplied input on the
//! consensus hot path, so the contract under test is: any truncation, corruption,
//! or unknown version yields a clean `Err` — never a panic, never a silent
//! misread. Corruption sweeps are exhaustive over the fixture (they are small),
//! not sampled, so a regression cannot hide behind an unlucky seed.

use std::path::PathBuf;

fn goldens_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("goldens")
}

fn read_golden(name: &str) -> Vec<u8> {
    let content = std::fs::read_to_string(goldens_dir().join(name))
        .unwrap_or_else(|_| panic!("fixture {name} is missing"));
    alloy::hex::decode(content.trim()).expect("fixture is valid hex")
}

/// The replay wire versions are plain RLP; damaged bytes must error, not panic.
/// (RLP decoding is alloy's job — this pins that our types never `expect` their
/// way past it.)
#[test]
fn damaged_replay_encodings_never_panic() {
    use alloy_rlp::Decodable;
    fn sweep<W: Decodable>(fixture: &str) {
        let bytes = read_golden(fixture);
        for len in 0..bytes.len() {
            let _ = W::decode(&mut &bytes[..len]);
        }
        for index in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[index] ^= 0xFF;
            let _ = W::decode(&mut damaged.as_slice());
        }
    }
    sweep::<zksync_os_wire::replays::v0::ReplayRecord>("replay_v0.hex");
    sweep::<zksync_os_wire::replays::v1::ReplayRecord>("replay_v1.hex");
    sweep::<zksync_os_wire::replays::v2::ReplayRecord>("replay_v2.hex");
    sweep::<zksync_os_wire::replays::v3::ReplayRecord>("replay_v3.hex");
}

/// The legacy replay versions are decode-forever: their committed bytes must keep
/// converting into the current storage record with the semantic fields intact —
/// an old node's write-ahead log replays through exactly this path.
#[test]
fn legacy_replay_versions_still_convert_to_the_storage_record() {
    use alloy_rlp::Decodable;
    use zksync_os_storage_api::ReplayRecord as StorageReplayRecord;
    use zksync_os_wire::replays::{self, WireReplayRecord};

    fn convert<W>(fixture: &str) -> StorageReplayRecord
    where
        W: Decodable + WireReplayRecord + TryInto<StorageReplayRecord>,
        <W as TryInto<StorageReplayRecord>>::Error: std::fmt::Debug,
    {
        let bytes = read_golden(fixture);
        let wire = W::decode(&mut bytes.as_slice()).expect("committed fixture must decode");
        assert_eq!(
            wire.block_number(),
            5,
            "{fixture}: the wire-level block number must survive the version"
        );
        wire.try_into().expect("signer recovery must succeed")
    }

    // v0 is the dummy version: only the block number survives, by design.
    let v0 = convert::<replays::v0::ReplayRecord>("replay_v0.hex");
    assert_eq!(v0.block_context.block_number, 5);

    // The current version answers through the same trait the network serves by.
    let v3 = convert::<replays::v3::ReplayRecord>("replay_v3.hex");
    assert_eq!(v3.block_context.block_number, 5);

    for (fixture, record) in [
        (
            "replay_v1.hex",
            convert::<replays::v1::ReplayRecord>("replay_v1.hex"),
        ),
        (
            "replay_v2.hex",
            convert::<replays::v2::ReplayRecord>("replay_v2.hex"),
        ),
    ] {
        assert_eq!(record.block_context.block_number, 5, "{fixture}");
        assert_eq!(record.block_context.chain_id, 270, "{fixture}");
        assert_eq!(record.block_context.timestamp, 1_700_000_001, "{fixture}");
        assert_eq!(record.previous_block_timestamp, 1_700_000_000, "{fixture}");
        assert_eq!(record.transactions.len(), 3, "{fixture}");
    }
}

#[cfg(feature = "consensus")]
mod consensus_formats {
    use super::read_golden;
    use commonware_codec::{EncodeSize, Error, Read, Write};
    use zksync_os_wire::{
        CommitteeMemberKeys, ConsensusBlock, DerivationOutcome, EpochTransition,
        FinalityCertificate, RegistryDerivation, SignatureScheme,
    };

    fn decode_block(bytes: &[u8]) -> Result<ConsensusBlock, Error> {
        ConsensusBlock::read_cfg(&mut &bytes[..], &0)
    }
    fn decode_certificate(bytes: &[u8]) -> Result<FinalityCertificate, Error> {
        FinalityCertificate::read_cfg(&mut &bytes[..], &())
    }
    fn decode_transition(bytes: &[u8]) -> Result<EpochTransition, Error> {
        EpochTransition::read_cfg(&mut &bytes[..], &())
    }
    fn decode_derivation(bytes: &[u8]) -> Result<RegistryDerivation, Error> {
        RegistryDerivation::read_cfg(&mut &bytes[..], &())
    }

    /// Every strict prefix of a valid encoding must be a clean error: these
    /// formats have no optional tail, so a truncated frame can never decode.
    /// (This is the test that pins every bounds check in the readers — a
    /// weakened bound turns the `Err` into an out-of-bounds panic.)
    #[test]
    fn every_truncation_reads_as_a_clean_error() {
        fn sweep<T>(fixture: &str, decode: impl Fn(&[u8]) -> Result<T, Error>) {
            let bytes = read_golden(fixture);
            for len in 0..bytes.len() {
                assert!(
                    decode(&bytes[..len]).is_err(),
                    "{fixture}: a {len}-byte prefix of the {}-byte encoding decoded",
                    bytes.len(),
                );
            }
        }
        sweep("consensus_block.hex", decode_block);
        sweep("finality_certificate_v1.hex", decode_certificate);
        sweep("epoch_transition_v1.hex", decode_transition);
        sweep("registry_derivation_v1.hex", decode_derivation);
    }

    /// Exhaustive single-byte corruption must never panic a decoder. A corrupted
    /// frame may still decode (most bytes are payload), but structural damage —
    /// lied-about lengths, bad enums — must surface as `Err`, not as an
    /// allocation blowup or slice panic.
    #[test]
    fn corrupted_bytes_never_panic_the_decoders() {
        fn sweep<T>(fixture: &str, decode: impl Fn(&[u8]) -> Result<T, Error>) {
            let bytes = read_golden(fixture);
            for index in 0..bytes.len() {
                for flip in [0xFF, 0x01, 0x80] {
                    let mut damaged = bytes.clone();
                    damaged[index] ^= flip;
                    let _ = decode(&damaged);
                }
            }
        }
        sweep("consensus_block.hex", decode_block);
        sweep("finality_certificate_v1.hex", decode_certificate);
        sweep("epoch_transition_v1.hex", decode_transition);
        sweep("registry_derivation_v1.hex", decode_derivation);
    }

    /// The version byte is the compatibility gate: anything but the released
    /// version is a refusal, so a future format can never be half-read by an
    /// old node.
    #[test]
    fn unknown_versions_are_rejected() {
        type Decodes = fn(&[u8]) -> bool;
        let formats: [(&str, Decodes); 3] = [
            ("finality_certificate_v1.hex", |bytes| {
                decode_certificate(bytes).is_ok()
            }),
            ("epoch_transition_v1.hex", |bytes| {
                decode_transition(bytes).is_ok()
            }),
            ("registry_derivation_v1.hex", |bytes| {
                decode_derivation(bytes).is_ok()
            }),
        ];
        for (fixture, decodes) in formats {
            let mut bytes = read_golden(fixture);
            assert!(decodes(&bytes), "{fixture}: the committed bytes decode");
            for version in [0u8, 2, 0x7f, 0xff] {
                bytes[0] = version;
                assert!(!decodes(&bytes), "{fixture}: version {version} decoded");
            }
        }
    }

    /// The block envelope's record slot dispatches on the record's first byte
    /// (EIP-2718 style). The two refusal shapes must keep their exact meanings:
    /// an empty record slot is a truncation, a low first byte is a future wire
    /// version this node does not speak — a routine no-vote, never conflated
    /// with garbage.
    #[test]
    fn record_version_dispatch_keeps_its_refusal_shapes() {
        fn envelope(record_bytes: &[u8]) -> Vec<u8> {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&7u64.to_be_bytes());
            bytes.extend_from_slice(&[0x11; 32]);
            bytes.push(1);
            bytes.extend_from_slice(&(record_bytes.len() as u64).to_be_bytes());
            bytes.extend_from_slice(record_bytes);
            bytes
        }

        // A declared-but-empty record is a truncated frame.
        assert!(matches!(
            decode_block(&envelope(&[])),
            Err(Error::EndOfBuffer)
        ));
        // A future version discriminator is exactly `InvalidEnum(discriminator)`.
        assert!(matches!(
            decode_block(&envelope(&[0x05, 0xAA])),
            Err(Error::InvalidEnum(0x05))
        ));
        // An unknown record flag is a refusal too.
        let mut flagged = envelope(&[]);
        flagged[40] = 2;
        assert!(decode_block(&flagged).is_err());
    }

    /// The genesis form (no record) is on the wire during migrations and must
    /// round-trip — the goldens only pin the record-carrying form.
    #[test]
    fn a_genesis_form_block_round_trips() {
        use commonware_cryptography::Digestible;
        let genesis = ConsensusBlock::genesis_at(100, alloy::primitives::B256::repeat_byte(0x2A));
        let mut encoded = Vec::with_capacity(genesis.encode_size());
        genesis.write(&mut encoded);
        assert_eq!(encoded.len(), genesis.encode_size());

        let decoded = decode_block(&encoded).expect("genesis form decodes");
        assert_eq!(decoded.digest(), genesis.digest());
        assert_eq!(decoded.height_u64(), 100);
        assert!(decoded.record().is_none());
        assert_eq!(decoded.encoded_record_len(), 0);
    }

    /// `encode_size` is the framing contract commonware sizes buffers with: it
    /// must equal the written length exactly, for every format.
    #[test]
    fn encode_size_matches_the_written_length() {
        fn check(value: &(impl Write + EncodeSize), what: &str) {
            let mut encoded = Vec::new();
            value.write(&mut encoded);
            assert_eq!(encoded.len(), value.encode_size(), "{what}");
        }

        let committed = read_golden("consensus_block.hex");
        let block = decode_block(&committed).expect("fixture decodes");
        check(&block, "consensus block (record form)");
        assert_eq!(
            block.encode_size(),
            committed.len(),
            "the fixture's own length is the size contract"
        );
        // The record length the size rule bounds is the envelope minus its
        // fixed frame: height (8) + parent digest (32) + flag (1) + length (8).
        assert_eq!(block.encoded_record_len(), committed.len() - 49);
        assert!(block.record().is_some(), "a record-form block carries it");

        check(
            &FinalityCertificate {
                scheme: SignatureScheme::Bls12381Multisig,
                epoch: 3,
                view: 77,
                block_digest: [0x1D; 32],
                committee_size: 5,
                signers: FinalityCertificate::bitmap_from_positions(5, &[0, 2, 3]),
                signature: (0..96).collect(),
            },
            "finality certificate",
        );
        check(
            &EpochTransition {
                epoch: 9,
                scheme: SignatureScheme::Bls12381Multisig,
                committee: vec![CommitteeMemberKeys {
                    network_key: [7; 32],
                    bls_key: [8; 48],
                }],
                first_finalized_digest: [0xB0; 32],
                first_finalized_view: 1,
            },
            "epoch transition",
        );
        check(
            &RegistryDerivation {
                epoch: 9,
                lookahead_height: 345_599,
                outcome: DerivationOutcome::Derived,
                committee: Vec::new(),
            },
            "registry derivation",
        );
    }

    /// The adversarial-length caps sit exactly on their limit: a 10 000-member
    /// committee still decodes (the cap is not a smaller number in disguise),
    /// and one past it is the explicit refusal — never a buffer error from
    /// trying to read members that were never there.
    #[test]
    fn absurd_committee_sizes_are_rejected_at_the_cap() {
        let members = |count: usize| -> Vec<CommitteeMemberKeys> {
            (0..count)
                .map(|i| CommitteeMemberKeys {
                    network_key: [(i % 251) as u8; 32],
                    bls_key: [(i % 251) as u8; 48],
                })
                .collect()
        };

        let transition = |count: usize| {
            let value = EpochTransition {
                epoch: 1,
                scheme: SignatureScheme::Bls12381Multisig,
                committee: members(count),
                first_finalized_digest: [0; 32],
                first_finalized_view: 0,
            };
            let mut encoded = Vec::with_capacity(value.encode_size());
            value.write(&mut encoded);
            decode_transition(&encoded)
        };
        assert!(transition(10_000).is_ok(), "the cap itself decodes");
        assert!(
            matches!(transition(10_001), Err(Error::Invalid(_, _))),
            "one past the cap is the explicit refusal"
        );

        let derivation = |count: usize| {
            let value = RegistryDerivation {
                epoch: 1,
                lookahead_height: 0,
                outcome: DerivationOutcome::Derived,
                committee: members(count),
            };
            let mut encoded = Vec::with_capacity(value.encode_size());
            value.write(&mut encoded);
            decode_derivation(&encoded)
        };
        assert!(derivation(10_000).is_ok(), "the cap itself decodes");
        assert!(
            matches!(derivation(10_001), Err(Error::Invalid(_, _))),
            "one past the cap is the explicit refusal"
        );
    }

    /// The signer bitmap is the certificate's audit surface: membership must be
    /// bounded by the committee size *and* the bitmap's actual length — padding
    /// bits beyond the committee never count, and a short bitmap never panics.
    #[test]
    fn signed_by_is_bounded_by_committee_and_bitmap() {
        let certificate = |committee_size, signers| FinalityCertificate {
            scheme: SignatureScheme::Bls12381Multisig,
            epoch: 0,
            view: 0,
            block_digest: [0; 32],
            committee_size,
            signers,
            signature: Vec::new(),
        };

        // All bitmap bits set, committee of 5: the padding bits are not members.
        let padded = certificate(5, vec![0xFF]);
        for index in 0..5 {
            assert!(padded.signed_by(index));
        }
        assert!(!padded.signed_by(5), "index == committee_size is outside");
        assert!(!padded.signed_by(7), "padding bits never count");

        // A bitmap shorter than the committee answers false, it does not index
        // out of bounds.
        let short = certificate(9, vec![0xFF]);
        assert!(short.signed_by(7));
        assert!(
            !short.signed_by(8),
            "beyond the bitmap is unsigned, not a panic"
        );
    }

    /// Era-relative height is what the consensus library schedules by: the era
    /// anchor is height zero, and the chain-absolute height stays separate.
    /// The committed block-1 fixture decoded under two different anchors pins
    /// the subtraction from both sides.
    #[test]
    fn consensus_height_is_relative_to_the_era_anchor() {
        use commonware_consensus::Heightable;
        use commonware_consensus::types::Height;

        let anchor = ConsensusBlock::genesis_at(100, alloy::primitives::B256::repeat_byte(0x2A));
        assert_eq!(anchor.height(), Height::new(0));
        assert_eq!(anchor.height_u64(), 100);

        let committed = read_golden("consensus_block.hex");
        let fresh_chain =
            ConsensusBlock::read_cfg(&mut committed.as_slice(), &0).expect("fixture decodes");
        assert_eq!(fresh_chain.height(), Height::new(1));
        let migrated =
            ConsensusBlock::read_cfg(&mut committed.as_slice(), &1).expect("fixture decodes");
        assert_eq!(migrated.height(), Height::new(0));
        assert_eq!(
            migrated.height_u64(),
            1,
            "chain-absolute height is unscaled"
        );
    }
}
