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
use zksync_os_consensus_core::era::EraHeight;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimBlock {
    height: u64,
    /// The chain height consensus counts from (see the wire block's field of the
    /// same name): the consensus library wants its genesis at height zero, so
    /// [`commonware_consensus::Heightable`] reports `height - era_anchor`.
    era_anchor: u64,
    parent: Digest,
    /// Stand-in for block content; the view the block was proposed in.
    seed: u64,
    /// Cached hash of the encoded block, computed on construction/decode.
    digest: Digest,
}

impl SimBlock {
    /// The block every simulated chain starts from. Identical on all validators.
    pub fn genesis() -> Self {
        Self::assemble(0, 0, Sha256::hash(b"sim-genesis-parent"), 0)
    }

    /// A genesis block anchored at `height` — the migration shape: consensus takes
    /// over a chain that already has `height` blocks of pre-consensus history, and
    /// this block *stands for* that history's tip. Every validator derives the
    /// identical anchor from the agreed height, exactly like the real node derives
    /// it from the agreed cutover block.
    pub fn anchor(height: u64) -> Self {
        Self::assemble(height, height, Sha256::hash(b"sim-anchor-parent"), height)
    }

    /// A child block on top of `parent`, with `seed` standing in for its content.
    pub fn child_of(parent: &SimBlock, seed: u64) -> Self {
        Self::assemble(parent.height + 1, parent.era_anchor, parent.digest(), seed)
    }

    /// A block with arbitrary linkage — byzantine fixtures use this to produce
    /// deliberately broken proposals (a parent digest nobody has, a height that does
    /// not follow the parent's). Honest code paths never need it.
    pub fn mislinked(height: u64, parent: Digest, seed: u64) -> Self {
        Self::assemble(height, 0, parent, seed)
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// The chain-absolute height (the encoded field), as opposed to the
    /// era-relative height consensus sees through `Heightable`.
    pub fn height_u64(&self) -> u64 {
        self.height
    }

    /// The era-relative height, in the typed coordinate era math takes.
    pub fn era_height(&self) -> EraHeight {
        EraHeight::from_chain(self.height, self.era_anchor)
    }

    fn assemble(height: u64, era_anchor: u64, parent: Digest, seed: u64) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(&height.to_be_bytes());
        hasher.update(parent.as_ref());
        hasher.update(&seed.to_be_bytes());
        Self {
            height,
            era_anchor,
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
    /// The era anchor (see the field): local era knowledge, not wire bytes.
    type Cfg = u64;

    fn read_cfg(buf: &mut impl Buf, era_anchor: &Self::Cfg) -> Result<Self, Error> {
        if buf.remaining() < 8 {
            return Err(Error::EndOfBuffer);
        }
        let height = buf.get_u64();
        let parent = Digest::read(buf)?;
        if buf.remaining() < 8 {
            return Err(Error::EndOfBuffer);
        }
        let seed = buf.get_u64();
        Ok(Self::assemble(height, *era_anchor, parent, seed))
    }
}

impl Digestible for SimBlock {
    type Digest = Digest;

    fn digest(&self) -> Digest {
        self.digest
    }
}

impl commonware_consensus::Heightable for SimBlock {
    /// Era-relative, mirroring the production wire block: the anchor is height zero.
    fn height(&self) -> Height {
        Height::new(self.height - self.era_anchor)
    }
}

impl commonware_consensus::Block for SimBlock {
    fn parent(&self) -> Digest {
        self.parent
    }
}
