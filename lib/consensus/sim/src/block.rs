//! The block type used in simulation: just enough structure to exercise consensus.
//!
//! A real block carries transactions and execution results; consensus only ever needs a
//! height, a parent link, a digest, and a wire encoding. [`SimBlock`] provides exactly
//! that, plus a `seed` so that two leaders building on the same parent (in different
//! views) produce distinguishable blocks — like real blocks with different content.

use bytes::{Buf, BufMut};
use commonware_codec::{EncodeSize, Error, Read, ReadExt, Write};
use commonware_consensus::types::Height;
use commonware_cryptography::sha256::Digest;
use commonware_cryptography::{Digestible, Hasher, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimBlock {
    height: u64,
    parent: Digest,
    /// Stand-in for block content; the view the block was proposed in.
    seed: u64,
    /// Cached hash of the encoded block, computed on construction/decode.
    digest: Digest,
}

impl SimBlock {
    /// The block every simulated chain starts from. Identical on all validators.
    pub fn genesis() -> Self {
        Self::assemble(0, Sha256::hash(b"sim-genesis-parent"), 0)
    }

    /// A child block on top of `parent`, with `seed` standing in for its content.
    pub fn child_of(parent: &SimBlock, seed: u64) -> Self {
        Self::assemble(parent.height + 1, parent.digest(), seed)
    }

    /// A block with arbitrary linkage — byzantine fixtures use this to produce
    /// deliberately broken proposals (a parent digest nobody has, a height that does
    /// not follow the parent's). Honest code paths never need it.
    pub fn mislinked(height: u64, parent: Digest, seed: u64) -> Self {
        Self::assemble(height, parent, seed)
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    fn assemble(height: u64, parent: Digest, seed: u64) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(&height.to_be_bytes());
        hasher.update(parent.as_ref());
        hasher.update(&seed.to_be_bytes());
        Self {
            height,
            parent,
            seed,
            digest: hasher.finalize(),
        }
    }
}

// The wire encoding: fixed-width fields, hashed as written. Consensus stores and gossips
// blocks through this encoding, so decode(encode(block)) must reproduce the digest.

impl Write for SimBlock {
    fn write(&self, buf: &mut impl BufMut) {
        buf.put_u64(self.height);
        self.parent.write(buf);
        buf.put_u64(self.seed);
    }
}

impl EncodeSize for SimBlock {
    fn encode_size(&self) -> usize {
        8 + self.parent.encode_size() + 8
    }
}

impl Read for SimBlock {
    type Cfg = ();

    fn read_cfg(buf: &mut impl Buf, _cfg: &Self::Cfg) -> Result<Self, Error> {
        if buf.remaining() < 8 {
            return Err(Error::EndOfBuffer);
        }
        let height = buf.get_u64();
        let parent = Digest::read(buf)?;
        if buf.remaining() < 8 {
            return Err(Error::EndOfBuffer);
        }
        let seed = buf.get_u64();
        Ok(Self::assemble(height, parent, seed))
    }
}

impl Digestible for SimBlock {
    type Digest = Digest;

    fn digest(&self) -> Digest {
        self.digest
    }
}

impl commonware_consensus::Heightable for SimBlock {
    fn height(&self) -> Height {
        Height::new(self.height)
    }
}

impl commonware_consensus::Block for SimBlock {
    fn parent(&self) -> Digest {
        self.parent
    }
}
