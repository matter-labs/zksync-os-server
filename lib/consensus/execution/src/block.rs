//! The consensus block: what validators propose, gossip, vote on, and archive.
//!
//! A consensus block is a thin envelope around the node's [`ReplayRecord`] — the
//! self-contained unit the node already uses to replay a block everywhere (write-ahead
//! log, external-node sync). The record carries the full signed transactions, the block
//! context the proposer chose (timestamp, fees, cursors), and the commitment to the
//! execution outcome (`block_output_hash`). The envelope adds what consensus needs on
//! top: the parent linkage by consensus digest, and a stable wire encoding whose hash
//! *is* the block's identity.
//!
//! Verifying a block therefore means: check the linkage, then re-execute the record
//! against the parent's state and require the recomputed outcome commitment to match.
//! Two validators that agree on a digest agree on the exact post-execution state.
//!
//! Encoding note: the embedded record uses the node's *versioned wire encoding* of
//! replay records (currently v3) — the same immutable-once-released format family the
//! external-node sync protocol ships, and the only representation of a record that is
//! safe to hash into an identity. Two consequences worth knowing:
//!
//! - The digest is independent of `node_version` (the wire format deliberately omits
//!   it): the same logical block built by different node releases has the same
//!   consensus identity.
//! - Moving a live network to a newer wire version is a coordinated, deliberate change
//!   by construction — a new version file, never an edit.
//!
//! The wire types currently live in the networking crate; consensus depending on it is
//! a known layering wart pending extraction of the encodings into a standalone crate.

use bytes::{Buf, BufMut};
use commonware_codec::{EncodeSize, Error, Read, ReadExt, Write};
use commonware_consensus::types::Height;
use commonware_cryptography::sha256::Digest;
use commonware_cryptography::{Digestible, Hasher, Sha256};
use std::sync::OnceLock;
use zksync_os_network::replays;
use zksync_os_storage_api::ReplayRecord;

#[derive(Debug, Clone)]
pub struct ConsensusBlock {
    height: u64,
    parent: Digest,
    /// The full replayable block. `None` exactly at height 0: the genesis block stands
    /// for the genesis state itself, which every validator derives locally from the
    /// genesis input — there is nothing to replay.
    record: Option<ReplayRecord>,
    /// Cached hash of the encoded block: the block's consensus identity.
    digest: Digest,
    /// Cached encoding of the record (computing it is not free; the digest, the wire,
    /// and size calculations all need the same bytes).
    encoded_record: OnceLock<Vec<u8>>,
}

impl ConsensusBlock {
    /// The chain root, derived from the genesis block hash so that chains with
    /// different genesis states get different consensus identities.
    pub fn genesis(genesis_block_hash: alloy::primitives::B256) -> Self {
        Self::from_parts(0, Digest::from(genesis_block_hash.0), None)
    }

    /// Envelope a produced/replayed record as the child of `parent`.
    pub fn from_record(parent: &ConsensusBlock, record: ReplayRecord) -> Self {
        assert_eq!(
            record.block_context.block_number,
            parent.height + 1,
            "record block number must directly follow its parent"
        );
        Self::from_parts(parent.height + 1, parent.digest, Some(record))
    }

    fn from_parts(height: u64, parent: Digest, record: Option<ReplayRecord>) -> Self {
        let mut block = Self {
            height,
            parent,
            record,
            digest: Digest::from([0u8; 32]),
            encoded_record: OnceLock::new(),
        };
        let mut encoded = Vec::with_capacity(block.encode_size());
        block.write(&mut encoded);
        let mut hasher = Sha256::new();
        hasher.update(&encoded);
        block.digest = hasher.finalize();
        block
    }

    /// The replayable payload; `None` only for the genesis block.
    pub fn record(&self) -> Option<&ReplayRecord> {
        self.record.as_ref()
    }

    pub fn height_u64(&self) -> u64 {
        self.height
    }
}

fn encode_record(record: &ReplayRecord) -> Vec<u8> {
    let wire: replays::v3::ReplayRecord = record.clone().into();
    alloy_rlp::encode(&wire)
}

fn decode_record(bytes: &[u8]) -> Result<ReplayRecord, Error> {
    let wire = <replays::v3::ReplayRecord as alloy_rlp::Decodable>::decode(&mut &bytes[..])
        .map_err(|err| Error::Wrapped("decoding wire replay record", err.into()))?;
    // Recovers transaction signers from their signatures — an invalid signature makes
    // the whole block undecodable, so a block that decodes carries only authentic
    // transactions. Stamps `node_version` with this node's own (it is metadata, not
    // block content: excluded from both the wire format and record equality).
    wire.try_into()
        .map_err(|err: alloy::consensus::crypto::RecoveryError| {
            Error::Wrapped("recovering transaction signers", err.into())
        })
}

impl ConsensusBlock {
    /// The record's encoding, computed once — used by both the digest and the wire.
    fn encoded_record(&self) -> &[u8] {
        static EMPTY: &[u8] = &[];
        match &self.record {
            None => EMPTY,
            Some(record) => self
                .encoded_record
                .get_or_init(|| encode_record(record))
                .as_slice(),
        }
    }
}

impl Write for ConsensusBlock {
    fn write(&self, buf: &mut impl BufMut) {
        buf.put_u64(self.height);
        self.parent.write(buf);
        match &self.record {
            None => buf.put_u8(0),
            Some(_) => {
                buf.put_u8(1);
                let encoded = self.encoded_record();
                buf.put_u64(encoded.len() as u64);
                buf.put_slice(encoded);
            }
        }
    }
}

impl EncodeSize for ConsensusBlock {
    fn encode_size(&self) -> usize {
        let record = match &self.record {
            None => 0,
            Some(_) => 8 + self.encoded_record().len(),
        };
        8 + self.parent.encode_size() + 1 + record
    }
}

impl Read for ConsensusBlock {
    type Cfg = ();

    fn read_cfg(buf: &mut impl Buf, _cfg: &Self::Cfg) -> Result<Self, Error> {
        if buf.remaining() < 8 {
            return Err(Error::EndOfBuffer);
        }
        let height = buf.get_u64();
        let parent = Digest::read(buf)?;
        if buf.remaining() < 1 {
            return Err(Error::EndOfBuffer);
        }
        let record = match buf.get_u8() {
            0 => None,
            1 => {
                if buf.remaining() < 8 {
                    return Err(Error::EndOfBuffer);
                }
                let length = buf.get_u64() as usize;
                if buf.remaining() < length {
                    return Err(Error::EndOfBuffer);
                }
                let mut encoded = vec![0u8; length];
                buf.copy_to_slice(&mut encoded);
                Some(decode_record(&encoded)?)
            }
            _ => return Err(Error::Invalid("ConsensusBlock", "bad record flag")),
        };
        Ok(Self::from_parts(height, parent, record))
    }
}

impl Digestible for ConsensusBlock {
    type Digest = Digest;

    fn digest(&self) -> Digest {
        self.digest
    }
}

impl commonware_consensus::Heightable for ConsensusBlock {
    fn height(&self) -> Height {
        Height::new(self.height)
    }
}

impl commonware_consensus::Block for ConsensusBlock {
    fn parent(&self) -> Digest {
        self.parent
    }
}
