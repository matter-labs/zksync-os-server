//! The finality certificate, in the node's own encoding.
//!
//! Consensus produces a certificate for every finalized block: which committee
//! members signed, and their aggregated signature. The consensus engine keeps its own
//! copies in its archives, in the encoding of the pinned consensus library — which
//! ships breaking releases monthly. Certificates are the one consensus artifact the
//! chain may need *forever* (they are what makes finality externally provable
//! later), so the node converts each one at finalization time into this sovereign
//! format and stores it under its own control. A consensus-library upgrade can then
//! never strand certificate history: the engine's archives are a rebuildable cache,
//! this record is the durable truth.
//!
//! The fields are semantic, not a pass-through of library types: scheme and epoch
//! identify the verification procedure and committee, the signer bitmap is plain
//! bits over committee positions, and the signature is the scheme's standard
//! serialization (for BLS12-381: the compressed point encoding defined by the BLS
//! standard itself, not by any library).
//!
//! What this record deliberately does *not* contain: the block height. The
//! certificate signs a consensus round and a block digest; height is chain metadata
//! the node indexes separately (digest → certificate, height → digest). And it makes
//! no promise of third-party *re-verification* across future scheme changes — that
//! is the externally-verifiable-finality project; this record is the prerequisite it
//! needs (the bytes plus the metadata to interpret them, never lost).

use bytes::{Buf, BufMut};
use commonware_codec::{EncodeSize, Error, Read, ReadExt, Write};

/// Identifies how a certificate's signature is built and verified.
///
/// A `u16` on the wire; new schemes append here — values are never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureScheme {
    /// BLS12-381 multi-signature (MinPk: 48-byte public keys, 96-byte signatures),
    /// aggregated over the signing committee members' individual votes.
    Bls12381Multisig = 1,
}

/// A finality certificate in the node's own versioned encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalityCertificate {
    /// How to interpret and verify `signature`.
    pub scheme: SignatureScheme,
    /// The consensus epoch — identifies the committee the signer bitmap indexes into.
    pub epoch: u64,
    /// The consensus view (round within the epoch) that finalized the block.
    pub view: u64,
    /// The consensus digest of the finalized block.
    pub block_digest: [u8; 32],
    /// Number of committee members in this epoch — the width of the signer bitmap.
    pub committee_size: u32,
    /// Bit `i` (little-endian within each byte: byte `i / 8`, bit `i % 8`) is set
    /// when committee position `i` contributed a signature.
    pub signers: Vec<u8>,
    /// The aggregated signature in the scheme's standard serialization.
    pub signature: Vec<u8>,
}

/// Leading version byte of the encoding below. Encodings are immutable once
/// released: changes add a new version, never edit this one.
const WIRE_VERSION: u8 = 1;

impl FinalityCertificate {
    /// Whether committee position `index` contributed a signature.
    pub fn signed_by(&self, index: u32) -> bool {
        let byte = (index / 8) as usize;
        let bit = index % 8;
        index < self.committee_size
            && byte < self.signers.len()
            && (self.signers[byte] >> bit) & 1 == 1
    }

    /// Builds the signer bitmap from committee positions.
    pub fn bitmap_from_positions(committee_size: u32, positions: &[u32]) -> Vec<u8> {
        let mut bitmap = vec![0u8; committee_size.div_ceil(8) as usize];
        for &position in positions {
            assert!(
                position < committee_size,
                "signer position {position} outside the committee of {committee_size}"
            );
            bitmap[(position / 8) as usize] |= 1 << (position % 8);
        }
        bitmap
    }
}

impl Write for FinalityCertificate {
    fn write(&self, buf: &mut impl BufMut) {
        buf.put_u8(WIRE_VERSION);
        buf.put_u16(self.scheme as u16);
        buf.put_u64(self.epoch);
        buf.put_u64(self.view);
        buf.put_slice(&self.block_digest);
        buf.put_u32(self.committee_size);
        buf.put_u32(self.signers.len() as u32);
        buf.put_slice(&self.signers);
        buf.put_u32(self.signature.len() as u32);
        buf.put_slice(&self.signature);
    }
}

impl EncodeSize for FinalityCertificate {
    fn encode_size(&self) -> usize {
        1 + 2 + 8 + 8 + 32 + 4 + 4 + self.signers.len() + 4 + self.signature.len()
    }
}

impl Read for FinalityCertificate {
    type Cfg = ();

    fn read_cfg(buf: &mut impl Buf, _cfg: &Self::Cfg) -> Result<Self, Error> {
        fn take(buf: &mut impl Buf, len: usize) -> Result<Vec<u8>, Error> {
            if buf.remaining() < len {
                return Err(Error::EndOfBuffer);
            }
            let mut bytes = vec![0u8; len];
            buf.copy_to_slice(&mut bytes);
            Ok(bytes)
        }

        let version = u8::read(buf)?;
        if version != WIRE_VERSION {
            return Err(Error::Invalid(
                "FinalityCertificate",
                "unknown encoding version",
            ));
        }
        if buf.remaining() < 2 {
            return Err(Error::EndOfBuffer);
        }
        let scheme = match buf.get_u16() {
            1 => SignatureScheme::Bls12381Multisig,
            _ => return Err(Error::Invalid("FinalityCertificate", "unknown scheme")),
        };
        if buf.remaining() < 8 + 8 {
            return Err(Error::EndOfBuffer);
        }
        let epoch = buf.get_u64();
        let view = buf.get_u64();
        let block_digest: [u8; 32] = take(buf, 32)?.try_into().expect("exactly 32 bytes");
        if buf.remaining() < 4 + 4 {
            return Err(Error::EndOfBuffer);
        }
        let committee_size = buf.get_u32();
        let signers_len = buf.get_u32() as usize;
        let signers = take(buf, signers_len)?;
        if buf.remaining() < 4 {
            return Err(Error::EndOfBuffer);
        }
        let signature_len = buf.get_u32() as usize;
        let signature = take(buf, signature_len)?;
        Ok(Self {
            scheme,
            epoch,
            view,
            block_digest,
            committee_size,
            signers,
            signature,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FinalityCertificate {
        FinalityCertificate {
            scheme: SignatureScheme::Bls12381Multisig,
            epoch: 0,
            view: 42,
            block_digest: [0x1D; 32],
            committee_size: 5,
            signers: FinalityCertificate::bitmap_from_positions(5, &[0, 2, 3, 4]),
            // Placeholder length; tests set a realistic 96-byte signature.
            signature: Vec::new(),
        }
    }

    #[test]
    fn roundtrips() {
        let mut certificate = sample();
        certificate.signature = vec![0xAB; 96];
        let mut encoded = Vec::with_capacity(certificate.encode_size());
        certificate.write(&mut encoded);
        assert_eq!(encoded.len(), certificate.encode_size());
        let decoded = FinalityCertificate::read_cfg(&mut encoded.as_slice(), &()).expect("decodes");
        assert_eq!(decoded, certificate);
    }

    #[test]
    fn bitmap_answers_signed_by() {
        let mut certificate = sample();
        certificate.signature = vec![0xAB; 96];
        assert!(certificate.signed_by(0));
        assert!(!certificate.signed_by(1));
        assert!(certificate.signed_by(2));
        assert!(certificate.signed_by(4));
        // Outside the committee: never signed, regardless of stray bits.
        assert!(!certificate.signed_by(5));
        assert!(!certificate.signed_by(100));
    }

    #[test]
    fn unknown_version_and_scheme_are_rejected() {
        let mut certificate = sample();
        certificate.signature = vec![0xAB; 96];
        let mut encoded = Vec::new();
        certificate.write(&mut encoded);

        let mut wrong_version = encoded.clone();
        wrong_version[0] = 2;
        assert!(FinalityCertificate::read_cfg(&mut wrong_version.as_slice(), &()).is_err());

        let mut wrong_scheme = encoded;
        wrong_scheme[1..3].copy_from_slice(&999u16.to_be_bytes());
        assert!(FinalityCertificate::read_cfg(&mut wrong_scheme.as_slice(), &()).is_err());
    }
}
