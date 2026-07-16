//! Identity digest for replay records.
//!
//! The digest commits to the record's *block identity* — the same field set as
//! [`ReplayRecord`]'s `PartialEq` — and deliberately excludes `node_version`: different node
//! versions can produce identical blocks (e.g. MN and EN during a rolling upgrade), and a
//! digest mismatch is treated as chain divergence, which a version skew is not.
//!
//! The digest is attached as object metadata when archiving, so any node can verify that an
//! already-archived record matches its own without holding the archive decryption key
//! (encrypted archive objects are randomized, so ciphertext comparison is impossible).

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use zksync_os_storage_api::{BlockContext, ReplayRecord};
use zksync_os_types::{BlockStartCursors, ProtocolSemanticVersion, ZkTransaction};

/// Object metadata key under which the identity digest is stored.
///
/// Lowercase because S3 lowercases user metadata keys; keeping one spelling everywhere lets
/// backends share lookup code.
pub const IDENTITY_DIGEST_METADATA_KEY: &str = "replay-record-identity-sha256";

/// Serialization view over the identity fields of a [`ReplayRecord`].
///
/// Field order and serde attributes mirror `ReplayRecord` so the canonical encoding stays
/// byte-stable; only `node_version` is omitted.
#[derive(Serialize)]
struct ReplayRecordIdentity<'a> {
    block_context: &'a BlockContext,
    transactions: &'a [ZkTransaction],
    previous_block_timestamp: u64,
    protocol_version: &'a ProtocolSemanticVersion,
    block_output_hash: &'a alloy::primitives::B256,
    force_preimages: &'a [(alloy::primitives::B256, Vec<u8>)],
    #[serde(flatten)]
    starting_cursors: &'a BlockStartCursors,
}

/// Computes the hex-encoded SHA-256 identity digest of a replay record.
pub fn replay_record_identity_digest(record: &ReplayRecord) -> String {
    // Exhaustive destructuring (no `..`) so that adding a field to `ReplayRecord` fails to
    // compile here, forcing a decision on whether the new field is part of block identity.
    let ReplayRecord {
        block_context,
        transactions,
        previous_block_timestamp,
        node_version: _,
        protocol_version,
        block_output_hash,
        force_preimages,
        starting_cursors,
    } = record;

    let identity = ReplayRecordIdentity {
        block_context,
        transactions,
        previous_block_timestamp: *previous_block_timestamp,
        protocol_version,
        block_output_hash,
        force_preimages,
        starting_cursors,
    };

    let mut hasher = HashWriter(Sha256::new());
    serde_json::to_writer(&mut hasher, &identity).expect("failed to encode replay record identity");
    alloy::hex::encode(hasher.0.finalize())
}

/// `io::Write` adapter so `serde_json` can stream into the hasher without buffering the
/// canonical encoding.
struct HashWriter(Sha256);

impl std::io::Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;

    fn test_replay_record(block_number: u64) -> ReplayRecord {
        ReplayRecord {
            block_context: zksync_os_storage_api::BlockContext {
                block_number,
                ..Default::default()
            },
            transactions: vec![],
            previous_block_timestamp: 0,
            node_version: "0.0.0".parse().unwrap(),
            protocol_version: "0.29.1".parse().unwrap(),
            block_output_hash: B256::ZERO,
            force_preimages: vec![],
            starting_cursors: Default::default(),
        }
    }

    #[test]
    fn digest_ignores_node_version() {
        let record = test_replay_record(7);
        let mut other_version = record.clone();
        other_version.node_version = "9.9.9".parse().unwrap();

        assert_eq!(
            replay_record_identity_digest(&record),
            replay_record_identity_digest(&other_version)
        );
    }

    #[test]
    fn digest_changes_with_block_identity() {
        let record = test_replay_record(7);
        let mut diverged = record.clone();
        diverged.block_output_hash = B256::with_last_byte(1);

        assert_ne!(
            replay_record_identity_digest(&record),
            replay_record_identity_digest(&diverged)
        );
    }

    #[test]
    fn digest_is_hex_sha256() {
        let digest = replay_record_identity_digest(&test_replay_record(7));

        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
