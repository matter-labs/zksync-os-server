use crate::hasher::Hasher;
use alloy::primitives::B256;
use sha2::{Digest, Sha256};

#[derive(Debug, Default, Clone, Copy)]
pub struct Sha256Hasher;

impl Hasher for Sha256Hasher {
    type Hash = B256;

    fn hash_bytes(&self, value: &[u8]) -> Self::Hash {
        let mut sha256 = Sha256::new();
        sha256.update(value);
        B256::from(<[u8; 32]>::from(sha256.finalize()))
    }

    fn compress(&self, lhs: &Self::Hash, rhs: &Self::Hash) -> Self::Hash {
        let mut hasher = Sha256::new();
        hasher.update(lhs);
        hasher.update(rhs);
        B256::from(<[u8; 32]>::from(hasher.finalize()))
    }
}
