//! The golden corpus: every released wire encoding, pinned byte-for-byte.
//!
//! Each fixture under `goldens/` is the committed truth for one format version. The
//! tests assert two directions: the canonical fixture *value* still encodes to
//! exactly the committed bytes (released encoders never drift), and the committed
//! bytes still decode and re-encode to themselves (decode-forever). A failure here
//! means a released wire format changed — which is never a fix, always a new
//! version file.
//!
//! Adding a fixture for a new format (never for changing a committed one):
//! `UPDATE_GOLDENS=1 cargo test -p zksync_os_wire --test goldens`

use alloy::consensus::transaction::Recovered;
use alloy::consensus::{SignableTransaction, TxEip1559};
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy_rlp::{Decodable, Encodable};
use std::path::PathBuf;
use std::str::FromStr;
use zksync_os_storage_api::{BlockContext, BlockHashes, ReplayRecord};
use zksync_os_types::{
    BlockStartCursors, ExecutionVersion, L1Envelope, L1PriorityEnvelope, L1Tx, L1UpgradeEnvelope,
    L2Envelope, ProtocolSemanticVersion, ZkTransaction,
};
use zksync_os_wire::replays;

fn goldens_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("goldens")
}

/// Compares `bytes` against the committed fixture, or writes the fixture when
/// `UPDATE_GOLDENS` is set and it does not exist yet. Committed fixtures are never
/// overwritten — an encoding change must add a new version, not edit a released one.
fn check_golden(name: &str, bytes: &[u8]) {
    let path = goldens_dir().join(name);
    let encoded = alloy::hex::encode(bytes);
    match std::fs::read_to_string(&path) {
        Ok(committed) => assert_eq!(
            committed.trim(),
            encoded,
            "encoding drifted from the committed fixture {name} — released wire \
             formats are immutable; add a new version instead",
        ),
        Err(_) if std::env::var("UPDATE_GOLDENS").is_ok() => {
            std::fs::create_dir_all(goldens_dir()).expect("create goldens dir");
            std::fs::write(&path, format!("{encoded}\n")).expect("write fixture");
        }
        Err(_) => panic!("fixture {name} is missing — generate it with UPDATE_GOLDENS=1"),
    }
}

fn read_golden(name: &str) -> Vec<u8> {
    let path = goldens_dir().join(name);
    let content = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("fixture {name} is missing — generate it with UPDATE_GOLDENS=1")
    });
    alloy::hex::decode(content.trim()).expect("fixture is valid hex")
}

/// The well-known development key: fixture signatures must be deterministic, and the
/// decode path's signer recovery needs something real to verify.
const SENDER_KEY: &str = "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110";

/// A deterministic signed transfer, field-for-field the shape the simulation harness
/// produces — the committed fixtures were generated with it and must never change.
fn signed_transfer(chain_id: u64, nonce: u64, value: U256) -> ZkTransaction {
    let sender = PrivateKeySigner::from_str(SENDER_KEY).expect("valid dev key");
    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 1_000_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(alloy::primitives::address!(
            "5e6D086F5eC079ADFF4FB3774CDf3e8D6a34F7E9"
        )),
        value,
        access_list: Default::default(),
        input: Default::default(),
    };
    let signature = sender
        .sign_hash_sync(&tx.signature_hash())
        .expect("signing cannot fail");
    let envelope: L2Envelope = tx.into_signed(signature).into();
    Recovered::new_unchecked(envelope, sender.address()).into()
}

/// A deterministic, fully-populated replay record: one upgrade transaction, one L1
/// priority transaction, one signed L2 transfer, force preimages, non-default
/// cursors, and a block-hash ring with content. Every encoding branch the current
/// formats have is exercised. (Interop system transactions do not occur under the
/// current formats' consensus usage and get their own fixture when they do.)
fn canonical_record() -> ReplayRecord {
    canonical_record_numbered(5)
}

/// Same record at a chosen height — the consensus-block fixture needs block 1 (its
/// construction asserts child-of-parent numbering against the genesis block).
fn canonical_record_numbered(block_number: u64) -> ReplayRecord {
    let protocol_version: ProtocolSemanticVersion = "0.31.0".parse().expect("valid version");
    let execution_version: ExecutionVersion =
        (&protocol_version).try_into().expect("supported version");

    let upgrade_envelope: L1UpgradeEnvelope = L1Envelope {
        inner: L1Tx {
            hash: B256::repeat_byte(0xAA),
            initiator: Address::repeat_byte(3),
            to: Address::repeat_byte(4),
            gas_limit: 72_000_000,
            gas_per_pubdata_byte_limit: 800,
            max_fee_per_gas: 0,
            max_priority_fee_per_gas: 0,
            nonce: 31,
            value: U256::ZERO,
            to_mint: U256::ZERO,
            refund_recipient: Address::repeat_byte(3),
            input: Bytes::from(vec![0xDE, 0xAD]),
            factory_deps: vec![B256::repeat_byte(0xBE)],
            marker: std::marker::PhantomData,
        },
    };
    let upgrade_tx: ZkTransaction = upgrade_envelope.into();
    let priority_envelope: L1PriorityEnvelope = L1Envelope {
        inner: L1Tx {
            hash: B256::repeat_byte(0x07),
            initiator: Address::repeat_byte(1),
            to: Address::repeat_byte(2),
            gas_limit: 500_000,
            gas_per_pubdata_byte_limit: 800,
            max_fee_per_gas: 0,
            max_priority_fee_per_gas: 0,
            nonce: 7,
            value: U256::from(1_000_000u64),
            to_mint: U256::from(1_000_000u64),
            refund_recipient: Address::repeat_byte(1),
            input: Bytes::new(),
            factory_deps: Vec::new(),
            marker: std::marker::PhantomData,
        },
    };
    let l1_tx: ZkTransaction = priority_envelope.into();
    let l2_tx: ZkTransaction = signed_transfer(270, 42, U256::from(12_345u64));

    ReplayRecord {
        block_context: BlockContext {
            chain_id: 270,
            block_number,
            block_hashes: BlockHashes::default()
                .push(B256::repeat_byte(0xA1))
                .push(B256::repeat_byte(0xA2)),
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
        transactions: vec![upgrade_tx, l1_tx, l2_tx],
        previous_block_timestamp: 1_700_000_000,
        // Deliberately absurd: node_version is metadata, not block content — it must
        // not influence any wire encoding or digest.
        node_version: semver::Version::new(9, 9, 9),
        protocol_version,
        block_output_hash: B256::repeat_byte(0xBB),
        force_preimages: vec![(B256::repeat_byte(5), vec![1, 2, 3])],
        starting_cursors: BlockStartCursors {
            l1_priority_id: 7,
            interop_root_id: 3,
            migration_number: 2,
            interop_fee_number: 1,
        },
    }
}

/// Pins one replay wire version: the canonical record's encoding matches the
/// committed fixture, and the committed bytes decode and re-encode to themselves.
fn pin_replay_version<W>(fixture: &str)
where
    W: From<ReplayRecord> + Encodable + Decodable,
{
    let wire: W = canonical_record().into();
    let mut encoded = Vec::new();
    wire.encode(&mut encoded);
    check_golden(fixture, &encoded);

    let committed = read_golden(fixture);
    let decoded = W::decode(&mut committed.as_slice()).expect("committed fixture must decode");
    let mut reencoded = Vec::new();
    decoded.encode(&mut reencoded);
    assert_eq!(
        committed, reencoded,
        "decode→encode of the committed fixture must reproduce it byte-exactly",
    );
}

#[test]
fn replay_v0_encoding_is_pinned() {
    pin_replay_version::<replays::v0::ReplayRecord>("replay_v0.hex");
}

#[test]
fn replay_v1_encoding_is_pinned() {
    pin_replay_version::<replays::v1::ReplayRecord>("replay_v1.hex");
}

#[test]
fn replay_v2_encoding_is_pinned() {
    pin_replay_version::<replays::v2::ReplayRecord>("replay_v2.hex");
}

#[test]
fn replay_v3_encoding_is_pinned() {
    pin_replay_version::<replays::v3::ReplayRecord>("replay_v3.hex");

    // The current version additionally converts all the way back to a storage
    // record: signer recovery works and the semantic fields survive the round trip.
    let committed = read_golden("replay_v3.hex");
    let decoded =
        replays::v3::ReplayRecord::decode(&mut committed.as_slice()).expect("fixture decodes");
    let storage: ReplayRecord = decoded.try_into().expect("signer recovery must succeed");
    let original = canonical_record();
    assert_eq!(storage.block_context, original.block_context);
    assert_eq!(storage.block_output_hash, original.block_output_hash);
    assert_eq!(storage.starting_cursors, original.starting_cursors);
    assert_eq!(storage.force_preimages, original.force_preimages);
    assert_eq!(storage.transactions.len(), original.transactions.len());
}

/// The consensus block envelope is a digest preimage: its bytes and the digest they
/// hash to are chain identity and must never drift.
#[cfg(feature = "consensus")]
#[test]
fn consensus_block_encoding_and_digest_are_pinned() {
    use commonware_codec::{EncodeSize, Write};
    use commonware_cryptography::Digestible;
    use zksync_os_wire::ConsensusBlock;

    let genesis = ConsensusBlock::genesis(B256::repeat_byte(0x11));
    let block = ConsensusBlock::from_record(&genesis, canonical_record_numbered(1));

    let mut encoded = Vec::with_capacity(block.encode_size());
    block.write(&mut encoded);
    check_golden("consensus_block.hex", &encoded);
    check_golden("consensus_block_digest.hex", block.digest().as_ref());

    // The digest of the decoded block matches the committed digest — decode must
    // reconstruct the identical identity.
    use commonware_codec::Read as _;
    let committed = read_golden("consensus_block.hex");
    let decoded = ConsensusBlock::read_cfg(&mut committed.as_slice(), &0).expect("fixture decodes");
    assert_eq!(
        decoded.digest().as_ref(),
        read_golden("consensus_block_digest.hex").as_slice(),
        "decoding the committed block must reproduce the committed digest",
    );
}

/// The finality-certificate encoding is a durable artifact (the chain's provable
/// finality trail) — pinned like every other released format.
#[cfg(feature = "consensus")]
#[test]
fn finality_certificate_encoding_is_pinned() {
    use commonware_codec::{EncodeSize, Read as _, Write};
    use zksync_os_wire::{FinalityCertificate, SignatureScheme};

    let certificate = FinalityCertificate {
        scheme: SignatureScheme::Bls12381Multisig,
        epoch: 3,
        view: 77,
        block_digest: [0x1D; 32],
        committee_size: 5,
        signers: FinalityCertificate::bitmap_from_positions(5, &[0, 2, 3, 4]),
        signature: (0..96).collect(),
    };
    let mut encoded = Vec::with_capacity(certificate.encode_size());
    certificate.write(&mut encoded);
    check_golden("finality_certificate_v1.hex", &encoded);

    let committed = read_golden("finality_certificate_v1.hex");
    let decoded =
        FinalityCertificate::read_cfg(&mut committed.as_slice(), &()).expect("fixture decodes");
    assert_eq!(decoded, certificate);
}

/// The epoch-transition encoding is the chain's committee custody trail — pinned
/// like every other released format.
#[cfg(feature = "consensus")]
#[test]
fn epoch_transition_encoding_is_pinned() {
    use commonware_codec::{EncodeSize, Read as _, Write};
    use zksync_os_wire::{CommitteeMemberKeys, EpochTransition, SignatureScheme};

    let transition = EpochTransition {
        epoch: 9,
        scheme: SignatureScheme::Bls12381Multisig,
        committee: (0u8..4)
            .map(|i| CommitteeMemberKeys {
                network_key: [i; 32],
                bls_key: [0x40 + i; 48],
            })
            .collect(),
        first_finalized_digest: [0xB0; 32],
        first_finalized_view: 1,
    };
    let mut encoded = Vec::with_capacity(transition.encode_size());
    transition.write(&mut encoded);
    check_golden("epoch_transition_v1.hex", &encoded);

    let committed = read_golden("epoch_transition_v1.hex");
    let decoded =
        EpochTransition::read_cfg(&mut committed.as_slice(), &()).expect("fixture decodes");
    assert_eq!(decoded, transition);
}

/// The registry-derivation encoding is the on-chain registry's durable derivation
/// trail — pinned like every other released format.
#[cfg(feature = "consensus")]
#[test]
fn registry_derivation_encoding_is_pinned() {
    use commonware_codec::{EncodeSize, Read as _, Write};
    use zksync_os_wire::{CommitteeMemberKeys, DerivationOutcome, RegistryDerivation};

    let derivation = RegistryDerivation {
        epoch: 9,
        lookahead_height: 345_599,
        outcome: DerivationOutcome::Derived,
        committee: (0u8..4)
            .map(|i| CommitteeMemberKeys {
                network_key: [i; 32],
                bls_key: [0x60 + i; 48],
            })
            .collect(),
    };
    let mut encoded = Vec::with_capacity(derivation.encode_size());
    derivation.write(&mut encoded);
    check_golden("registry_derivation_v1.hex", &encoded);

    let committed = read_golden("registry_derivation_v1.hex");
    let decoded =
        RegistryDerivation::read_cfg(&mut committed.as_slice(), &()).expect("fixture decodes");
    assert_eq!(decoded, derivation);
}
