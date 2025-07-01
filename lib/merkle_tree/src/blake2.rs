use alloy::primitives::B256;
use blake2::{Blake2s256, Digest};

/// Definition of hasher suitable for calculating state hash.
pub trait Hasher {
    type Hash: AsRef<[u8]>;

    /// Gets the hash of the byte sequence.
    fn hash_bytes(&self, value: &[u8]) -> Self::Hash;

    /// Merges two hashes into one.
    fn compress(&self, lhs: &Self::Hash, rhs: &Self::Hash) -> Self::Hash;
}

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
