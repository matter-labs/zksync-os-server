//! Big-calldata transactions: random bytes to a sink address. Wire, mempool
//! and gossip bandwidth under consensus, plus pubdata-heavy blocks.

use super::{Expectation, TxPlan, Workload};
use alloy::primitives::{Address, Bytes, U256, keccak256};
use rand08::{Rng as _, rngs::StdRng};

pub struct Blobs {
    bytes_per_tx: usize,
}

impl Blobs {
    pub fn new(blob_kib: u64) -> Blobs {
        Blobs {
            bytes_per_tx: (blob_kib as usize) * 1024,
        }
    }
}

impl Workload for Blobs {
    fn name(&self) -> &'static str {
        "blobs"
    }

    fn fire(&mut self, rng: &mut StdRng) -> TxPlan {
        let mut payload = vec![0u8; self.bytes_per_tx];
        rng.fill(payload.as_mut_slice());
        TxPlan {
            to: Address::from_word(keccak256(b"chaos-blob-sink")),
            value: U256::from(1u64),
            input: Bytes::from(payload),
            // Calldata dominates: ~16 gas per (nonzero) byte, plus headroom for
            // the transfer itself and this chain's per-byte pubdata pricing.
            gas_limit: 200_000 + (self.bytes_per_tx as u64) * 20,
            expect: Expectation::Accept,
        }
    }
}
