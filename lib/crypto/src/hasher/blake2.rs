use alloy::primitives::B256;
use blake2::{Blake2s256, Digest};

use crate::hasher::Hasher;

#[derive(Default, Clone, Debug)]
pub struct Blake2Hasher;

impl Hasher for Blake2Hasher {
    type Hash = B256;

    fn hash_bytes(&self, value: &[u8]) -> B256 {
        let mut hasher = Blake2s256::new();
        hasher.update(value);
        B256::from(<[u8; 32]>::from(hasher.finalize()))
    }

    fn compress(&self, lhs: &B256, rhs: &B256) -> B256 {
        let mut hasher = Blake2s256::new();
        hasher.update(lhs);
        hasher.update(rhs);
        B256::from(<[u8; 32]>::from(hasher.finalize()))
    }
}
