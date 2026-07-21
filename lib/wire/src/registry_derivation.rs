//! The registry derivation record: what the on-chain validator registry said the
//! committee for an epoch should be, read at that epoch's fixed lookahead height.
//!
//! The committee custody trail ([`crate::EpochTransition`]) records which committee
//! *actually held* an epoch. This record is its forward-looking sibling for the
//! on-chain registry: at every epoch's lookahead boundary the node reads the
//! registry contract's storage out of its own chain state and records the outcome —
//! the derived committee, or the fact that the registry had nothing usable and the
//! previous committee was carried forward. Because the lookahead height is a fixed,
//! deterministic function of the epoch, the outcome is a pure function of finalized
//! chain state: every honest node derives identical bytes, whether it derived live
//! at the boundary or years later from historical state.
//!
//! The record makes the derivation trail durable and auditable, and it is what
//! restarts and floor rebuilds replay instead of re-deriving — chain state at old
//! lookahead heights may be pruned, but the recorded outcome never is.
//!
//! Records are written first-observed-wins, like the custody trail: the outcome at
//! a given height is a chain fact, and a trail that could be rewritten would not be
//! an audit trail.

use crate::epoch_transition::CommitteeMemberKeys;
use bytes::{Buf, BufMut};
use commonware_codec::{EncodeSize, Error, Read, ReadExt, Write};

/// What the registry read at the lookahead height yielded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivationOutcome {
    /// The registry held a valid schedule entry for the epoch; the recorded
    /// committee is the registry's.
    Derived = 1,
    /// The registry was readable but held no schedule entry applicable to the
    /// epoch (deployed-but-unpopulated); the recorded committee is the carried
    /// previous one.
    CarriedNoEntry = 2,
    /// The registry refused validation (unknown layout version, malformed or
    /// reused keys, invalid proof of possession, a broken schedule — the reasons
    /// live in logs); the recorded committee is the carried previous one.
    CarriedRefused = 3,
}

/// One registry derivation, in the node's own versioned encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryDerivation {
    /// The epoch this derivation decides (or would decide, in shadow mode).
    pub epoch: u64,
    /// The chain-absolute height whose state the registry was read at — the
    /// epoch's fixed lookahead boundary.
    pub lookahead_height: u64,
    pub outcome: DerivationOutcome,
    /// The committee in effect for the epoch under this derivation: the registry's
    /// on `Derived`, the carried previous one otherwise. Order is part of the
    /// agreement (certificate signer bitmaps index into it).
    pub committee: Vec<CommitteeMemberKeys>,
}

/// Leading version byte of the encoding below. Encodings are immutable once
/// released: changes add a new version, never edit this one.
const WIRE_VERSION: u8 = 1;

impl Write for RegistryDerivation {
    fn write(&self, buf: &mut impl BufMut) {
        buf.put_u8(WIRE_VERSION);
        buf.put_u64(self.epoch);
        buf.put_u64(self.lookahead_height);
        buf.put_u8(self.outcome as u8);
        buf.put_u32(self.committee.len() as u32);
        for member in &self.committee {
            buf.put_slice(&member.network_key);
            buf.put_slice(&member.bls_key);
        }
    }
}

impl EncodeSize for RegistryDerivation {
    fn encode_size(&self) -> usize {
        1 + 8 + 8 + 1 + 4 + self.committee.len() * (32 + 48)
    }
}

impl Read for RegistryDerivation {
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
                "RegistryDerivation",
                "unknown encoding version",
            ));
        }
        if buf.remaining() < 8 + 8 + 1 + 4 {
            return Err(Error::EndOfBuffer);
        }
        let epoch = buf.get_u64();
        let lookahead_height = buf.get_u64();
        let outcome = match buf.get_u8() {
            1 => DerivationOutcome::Derived,
            2 => DerivationOutcome::CarriedNoEntry,
            3 => DerivationOutcome::CarriedRefused,
            _ => return Err(Error::Invalid("RegistryDerivation", "unknown outcome")),
        };
        let count = buf.get_u32() as usize;
        // Committees are small (tens of members); the cap only bounds adversarial
        // lengths before allocation.
        if count > 10_000 {
            return Err(Error::Invalid(
                "RegistryDerivation",
                "absurd committee size",
            ));
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
            lookahead_height,
            outcome,
            committee,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RegistryDerivation {
        RegistryDerivation {
            epoch: 9,
            lookahead_height: 345_599,
            outcome: DerivationOutcome::Derived,
            committee: (0u8..4)
                .map(|i| CommitteeMemberKeys {
                    network_key: [i; 32],
                    bls_key: [0x60 + i; 48],
                })
                .collect(),
        }
    }

    #[test]
    fn roundtrips_in_every_outcome() {
        for outcome in [
            DerivationOutcome::Derived,
            DerivationOutcome::CarriedNoEntry,
            DerivationOutcome::CarriedRefused,
        ] {
            let record = RegistryDerivation {
                outcome,
                ..sample()
            };
            let mut encoded = Vec::with_capacity(record.encode_size());
            record.write(&mut encoded);
            assert_eq!(encoded.len(), record.encode_size());
            let decoded =
                RegistryDerivation::read_cfg(&mut encoded.as_slice(), &()).expect("decodes");
            assert_eq!(decoded, record);
        }
    }

    #[test]
    fn unknown_version_and_outcome_are_rejected() {
        let record = sample();
        let mut encoded = Vec::new();
        record.write(&mut encoded);

        let mut wrong_version = encoded.clone();
        wrong_version[0] = 2;
        assert!(RegistryDerivation::read_cfg(&mut wrong_version.as_slice(), &()).is_err());

        let mut wrong_outcome = encoded;
        wrong_outcome[17] = 9;
        assert!(RegistryDerivation::read_cfg(&mut wrong_outcome.as_slice(), &()).is_err());
    }

    #[test]
    fn truncation_is_rejected_at_every_length() {
        let record = sample();
        let mut encoded = Vec::new();
        record.write(&mut encoded);
        for len in 0..encoded.len() {
            assert!(
                RegistryDerivation::read_cfg(&mut &encoded[..len], &()).is_err(),
                "truncation to {len} bytes must not decode"
            );
        }
    }
}
