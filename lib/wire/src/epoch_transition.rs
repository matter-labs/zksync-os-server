//! The epoch transition record: which committee took over at which epoch.
//!
//! The committee schedule is *forward-looking configuration* — every operator
//! deploys it before an activation epoch arrives, and nothing on the wire decides
//! it. What configuration cannot provide is an audit trail of what actually
//! happened: which committee, concretely, held epoch E on *this* chain, and where
//! its responsibility began. This record is that trail. The node writes one per
//! epoch it observes consensus enter, keyed by epoch, next to the finality
//! certificates in the finality store — so the chain's custody history (era anchor
//! → committee per epoch → certificate per block) is reconstructible from the
//! node's own durable data, with no reference to any config file that may since
//! have been rewritten.
//!
//! Honest semantics, stated plainly: the *authorization* of a committee change is
//! mutual configuration — operators deployed matching schedules, and this record
//! does not (cannot) prove they were entitled to. What it proves is custody: the
//! named committee is the one whose certificates finalize this epoch's blocks, and
//! `first_finalized_digest` pins where that responsibility observably began. For a
//! boundary crossing that digest is the previous epoch's final block, re-certified
//! by the new committee as its first act (the handoff protocol); for the era's
//! first epoch it is the era's first finalized block. Trustless verification of
//! handoffs — a chain of records a light client could walk — arrives with the
//! registry/threshold work; the scheme tag and committee list here are shaped so
//! that evolution adds versions, not migrations.
//!
//! The record is written at the *first observed* finalization of each epoch and
//! never rewritten: replays and backfills that re-report the same epoch leave the
//! original record in place. An audit trail that could be overwritten would not be
//! one.

use crate::finality_certificate::SignatureScheme;
use bytes::{Buf, BufMut};
use commonware_codec::{EncodeSize, Error, Read, ReadExt, Write};

/// One committee member's keys: the network identity and the consensus signing key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitteeMemberKeys {
    /// ed25519 public key — the validator's network identity.
    pub network_key: [u8; 32],
    /// BLS12-381 public key (MinPk: 48 bytes compressed) — signs consensus votes.
    pub bls_key: [u8; 48],
}

/// The epoch transition record in the node's own versioned encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochTransition {
    /// The epoch this committee holds (from its first block to the epoch's last).
    pub epoch: u64,
    /// How this committee's certificates are built and verified — the scheme the
    /// signer bitmap and signatures of this epoch's [`crate::FinalityCertificate`]s
    /// are interpreted under.
    pub scheme: SignatureScheme,
    /// The committee, in agreed order: certificate signer bitmaps index into this.
    pub committee: Vec<CommitteeMemberKeys>,
    /// The digest of the first block this node observed the committee finalize.
    /// At a boundary crossing this is the previous epoch's final block (the
    /// handoff re-certification); for the era's first epoch, the era's first block.
    pub first_finalized_digest: [u8; 32],
    /// The view of that first observed finalization — with `epoch`, locates the
    /// certificate itself in the finality store.
    pub first_finalized_view: u64,
}

/// Leading version byte of the encoding below. Encodings are immutable once
/// released: changes add a new version, never edit this one.
const WIRE_VERSION: u8 = 1;

impl Write for EpochTransition {
    fn write(&self, buf: &mut impl BufMut) {
        buf.put_u8(WIRE_VERSION);
        buf.put_u64(self.epoch);
        buf.put_u16(self.scheme as u16);
        buf.put_slice(&self.first_finalized_digest);
        buf.put_u64(self.first_finalized_view);
        buf.put_u32(self.committee.len() as u32);
        for member in &self.committee {
            buf.put_slice(&member.network_key);
            buf.put_slice(&member.bls_key);
        }
    }
}

impl EncodeSize for EpochTransition {
    fn encode_size(&self) -> usize {
        1 + 8 + 2 + 32 + 8 + 4 + self.committee.len() * (32 + 48)
    }
}

impl Read for EpochTransition {
    type Cfg = ();

    fn read_cfg(buf: &mut impl Buf, _cfg: &Self::Cfg) -> Result<Self, Error> {
        fn take<const N: usize>(buf: &mut impl Buf) -> Result<[u8; N], Error> {
            if buf.remaining() < N {
                return Err(Error::EndOfBuffer);
            }
            let mut bytes = [0u8; N];
            buf.copy_to_slice(&mut bytes);
            Ok(bytes)
        }

        let version = u8::read(buf)?;
        if version != WIRE_VERSION {
            return Err(Error::Invalid(
                "EpochTransition",
                "unknown encoding version",
            ));
        }
        if buf.remaining() < 8 + 2 {
            return Err(Error::EndOfBuffer);
        }
        let epoch = buf.get_u64();
        let scheme = match buf.get_u16() {
            1 => SignatureScheme::Bls12381Multisig,
            _ => return Err(Error::Invalid("EpochTransition", "unknown scheme")),
        };
        let first_finalized_digest = take::<32>(buf)?;
        if buf.remaining() < 8 + 4 {
            return Err(Error::EndOfBuffer);
        }
        let first_finalized_view = buf.get_u64();
        let count = buf.get_u32() as usize;
        // Committees are small (tens of members); the cap only bounds adversarial
        // lengths before allocation.
        if count > 10_000 {
            return Err(Error::Invalid("EpochTransition", "absurd committee size"));
        }
        let mut committee = Vec::with_capacity(count);
        for _ in 0..count {
            committee.push(CommitteeMemberKeys {
                network_key: take::<32>(buf)?,
                bls_key: take::<48>(buf)?,
            });
        }
        Ok(Self {
            epoch,
            scheme,
            committee,
            first_finalized_digest,
            first_finalized_view,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> EpochTransition {
        EpochTransition {
            epoch: 7,
            scheme: SignatureScheme::Bls12381Multisig,
            committee: (0u8..4)
                .map(|i| CommitteeMemberKeys {
                    network_key: [i; 32],
                    bls_key: [0x40 + i; 48],
                })
                .collect(),
            first_finalized_digest: [0xB0; 32],
            first_finalized_view: 1,
        }
    }

    #[test]
    fn roundtrips() {
        let record = sample();
        let mut encoded = Vec::with_capacity(record.encode_size());
        record.write(&mut encoded);
        assert_eq!(encoded.len(), record.encode_size());
        let decoded = EpochTransition::read_cfg(&mut encoded.as_slice(), &()).expect("decodes");
        assert_eq!(decoded, record);
    }

    #[test]
    fn unknown_version_and_scheme_are_rejected() {
        let record = sample();
        let mut encoded = Vec::new();
        record.write(&mut encoded);

        let mut wrong_version = encoded.clone();
        wrong_version[0] = 2;
        assert!(EpochTransition::read_cfg(&mut wrong_version.as_slice(), &()).is_err());

        let mut wrong_scheme = encoded;
        wrong_scheme[9..11].copy_from_slice(&999u16.to_be_bytes());
        assert!(EpochTransition::read_cfg(&mut wrong_scheme.as_slice(), &()).is_err());
    }

    #[test]
    fn truncation_is_rejected_at_every_length() {
        let record = sample();
        let mut encoded = Vec::new();
        record.write(&mut encoded);
        for len in 0..encoded.len() {
            assert!(
                EpochTransition::read_cfg(&mut &encoded[..len], &()).is_err(),
                "truncation to {len} bytes must not decode"
            );
        }
    }
}
