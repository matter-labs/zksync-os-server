//! RocksDB-backed persistence for FRI proofs.
//!
//! A proof is a binary blob (`Vec<u8>`) identified by its block number.
//!

use zksync_storage::db::{NamedColumnFamily, WriteBatch};
use zksync_storage::RocksDB;

/// Column family set for proof storage.
#[derive(Copy, Clone, Debug)]
pub enum ProofColumnFamily {
    /// Maps `block_number (u64, BE)` → raw proof bytes.
    Proofs,
}

impl NamedColumnFamily for ProofColumnFamily {
    const DB_NAME: &'static str = "proof_storage";
    const ALL: &'static [Self] = &[ProofColumnFamily::Proofs];

    fn name(&self) -> &'static str {
        match self {
            ProofColumnFamily::Proofs => "proofs",
        }
    }
}

/// Thin, clonable wrapper around a RocksDB instance.
#[derive(Clone, Debug)]
pub struct ProofStorage {
    db: RocksDB<ProofColumnFamily>,
}

impl ProofStorage {
    pub fn new(db: RocksDB<ProofColumnFamily>) -> Self {
        Self { db }
    }

    /// Persist a proof. Overwrites any existing entry for the same block.
    pub fn save_proof(&self, block_number: u64, proof: &[u8]) -> anyhow::Result<()> {
        let key = block_number.to_be_bytes();
        let mut batch: WriteBatch<'_, ProofColumnFamily> = self.db.new_write_batch();
        batch.put_cf(ProofColumnFamily::Proofs, &key, proof);
        self.db.write(batch)?;
        Ok(())
    }

    /// Persist a proof with prover ID label. Overwrites any existing entry for the same block.
    pub fn save_proof_with_prover(&self, block_number: u64, proof: &[u8], prover_id: &str) -> anyhow::Result<()> {
        let key = block_number.to_be_bytes();
        let mut batch: WriteBatch<'_, ProofColumnFamily> = self.db.new_write_batch();
        batch.put_cf(ProofColumnFamily::Proofs, &key, proof);
        tracing::info!(block_number, prover_id, "Saving proof with prover ID");
        self.db.write(batch)?;
        Ok(())
    }

    /// Loads a proof for `block_number`, if present.
    pub fn get_proof(&self, block_number: u64) -> Option<Vec<u8>> {
        let key = block_number.to_be_bytes();
        self.db
            .get_cf(ProofColumnFamily::Proofs, &key)
            .expect("DB read failure")
    }

    pub fn get_blocks_with_proof(&self) -> Vec<u64> {
        self.db
            .prefix_iterator_cf(ProofColumnFamily::Proofs, &[])
            .map(|(key, _)| {
                let arr: [u8; 8] = key.as_ref()[..8].try_into().expect("key length");
                u64::from_be_bytes(arr)
            })
            .collect()
    }
}
