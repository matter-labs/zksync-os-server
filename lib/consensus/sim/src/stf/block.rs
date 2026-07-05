//! A consensus block carrying real transactions.
//!
//! This is a first sketch of what the production consensus block will look like: a
//! small header binding the block into the chain (height, parent digest, timestamp),
//! the full signed transactions, and two commitments to the *outcome* of executing
//! them — the execution-layer header hash and the execution-outcome hash. A verifier
//! re-executes the transactions and checks both commitments; agreeing on a block digest
//! therefore means agreeing on the exact post-execution state.

use alloy::eips::{Decodable2718, Encodable2718};
use alloy::primitives::B256;
use bytes::{Buf, BufMut};
use commonware_codec::{EncodeSize, Error, Read, ReadExt, Write};
use commonware_consensus::types::Height;
use commonware_cryptography::sha256::Digest;
use commonware_cryptography::{Digestible, Hasher, Sha256};
use zksync_os_types::{L2Envelope, L2Transaction};

#[derive(Debug, Clone)]
pub struct StfBlock {
    height: u64,
    parent: Digest,
    /// Chosen by the proposer; also what makes two proposals on the same parent in
    /// different views distinct blocks.
    timestamp: u64,
    /// The block content: full signed L2 transactions.
    txs: Vec<L2Transaction>,
    /// Hash of the execution-layer block header produced by running `txs`.
    header_hash: B256,
    /// Commitment to the execution outcome (header, per-tx status/gas, storage writes).
    /// A verifier recomputes this by re-executing and must get the same value.
    block_output_hash: B256,
    /// Cached hash of the encoded block.
    digest: Digest,
}

impl StfBlock {
    /// The chain root. Carries no transactions and no execution outcome — it stands for
    /// the genesis state itself, which every validator derives locally.
    pub fn genesis(genesis_header_hash: B256) -> Self {
        Self::assemble(
            0,
            Sha256::hash(b"stf-genesis-parent"),
            0,
            Vec::new(),
            genesis_header_hash,
            B256::ZERO,
        )
    }

    /// A chain root anchored at `height` — the migration shape: this block stands for
    /// the tip of `height` blocks of pre-consensus history, identified by that tip's
    /// real execution-layer header hash. Every validator derives the identical anchor
    /// from the agreed (height, hash) pair.
    pub fn anchor(height: u64, timestamp: u64, tip_header_hash: B256) -> Self {
        Self::assemble(
            height,
            Sha256::hash(b"stf-anchor-parent"),
            timestamp,
            Vec::new(),
            tip_header_hash,
            B256::ZERO,
        )
    }

    pub fn assemble(
        height: u64,
        parent: Digest,
        timestamp: u64,
        txs: Vec<L2Transaction>,
        header_hash: B256,
        block_output_hash: B256,
    ) -> Self {
        let mut block = Self {
            height,
            parent,
            timestamp,
            txs,
            header_hash,
            block_output_hash,
            digest: Digest::from([0u8; 32]),
        };
        let mut hasher = Sha256::new();
        let mut encoded = Vec::with_capacity(block.encode_size());
        block.write(&mut encoded);
        hasher.update(&encoded);
        block.digest = hasher.finalize();
        block
    }

    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    pub fn txs(&self) -> &[L2Transaction] {
        &self.txs
    }

    pub fn header_hash(&self) -> B256 {
        self.header_hash
    }

    pub fn block_output_hash(&self) -> B256 {
        self.block_output_hash
    }

    pub fn height_u64(&self) -> u64 {
        self.height
    }
}

impl Write for StfBlock {
    fn write(&self, buf: &mut impl BufMut) {
        buf.put_u64(self.height);
        self.parent.write(buf);
        buf.put_u64(self.timestamp);
        buf.put_u32(self.txs.len() as u32);
        for tx in &self.txs {
            let encoded = tx.inner().encoded_2718();
            buf.put_u32(encoded.len() as u32);
            buf.put_slice(&encoded);
            buf.put_slice(tx.signer().as_slice());
        }
        buf.put_slice(self.header_hash.as_slice());
        buf.put_slice(self.block_output_hash.as_slice());
    }
}

impl EncodeSize for StfBlock {
    fn encode_size(&self) -> usize {
        let txs: usize = self
            .txs
            .iter()
            .map(|tx| 4 + tx.inner().encoded_2718().len() + 20)
            .sum();
        8 + self.parent.encode_size() + 8 + 4 + txs + 32 + 32
    }
}

impl Read for StfBlock {
    type Cfg = ();

    fn read_cfg(buf: &mut impl Buf, _cfg: &Self::Cfg) -> Result<Self, Error> {
        fn take_u64(buf: &mut impl Buf) -> Result<u64, Error> {
            if buf.remaining() < 8 {
                return Err(Error::EndOfBuffer);
            }
            Ok(buf.get_u64())
        }
        fn take_u32(buf: &mut impl Buf) -> Result<u32, Error> {
            if buf.remaining() < 4 {
                return Err(Error::EndOfBuffer);
            }
            Ok(buf.get_u32())
        }
        fn take_bytes(buf: &mut impl Buf, len: usize) -> Result<Vec<u8>, Error> {
            if buf.remaining() < len {
                return Err(Error::EndOfBuffer);
            }
            let mut bytes = vec![0u8; len];
            buf.copy_to_slice(&mut bytes);
            Ok(bytes)
        }

        let height = take_u64(buf)?;
        let parent = Digest::read(buf)?;
        let timestamp = take_u64(buf)?;
        let tx_count = take_u32(buf)?;
        let mut txs = Vec::with_capacity(tx_count as usize);
        for _ in 0..tx_count {
            let tx_len = take_u32(buf)? as usize;
            let tx_bytes = take_bytes(buf, tx_len)?;
            let envelope = L2Envelope::decode_2718(&mut tx_bytes.as_slice())
                .map_err(|err| Error::Wrapped("decoding transaction envelope", err.into()))?;
            let signer_bytes = take_bytes(buf, 20)?;
            let signer = alloy::primitives::Address::from_slice(&signer_bytes);
            // The signer travels with the transaction instead of being recovered from
            // the signature — good enough for simulation. The production block format
            // must recover and verify signers instead of trusting the wire.
            txs.push(L2Transaction::new_unchecked(envelope, signer));
        }
        let header_hash = B256::from_slice(&take_bytes(buf, 32)?);
        let block_output_hash = B256::from_slice(&take_bytes(buf, 32)?);
        Ok(Self::assemble(
            height,
            parent,
            timestamp,
            txs,
            header_hash,
            block_output_hash,
        ))
    }
}

impl Digestible for StfBlock {
    type Digest = Digest;

    fn digest(&self) -> Digest {
        self.digest
    }
}

impl commonware_consensus::Heightable for StfBlock {
    fn height(&self) -> Height {
        Height::new(self.height)
    }
}

impl commonware_consensus::Block for StfBlock {
    fn parent(&self) -> Digest {
        self.parent
    }
}
