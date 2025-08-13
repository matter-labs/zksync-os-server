//! RocksDB-backed persistence for Batch metadata and FRI proofs.
//! May be extracted to a separate service later on (aka FRI Cache)
//!

use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof};
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::{NamedColumnFamily, WriteBatch};

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

    /// Persist a BatchWithProof. Overwrites any existing entry for the same batch.
    /// Doesn't allow gaps - if a proof for batch `n` is missing, then no proof for batch `n+1` is allowed.
    pub fn save_proof(&self, value: &BatchEnvelope<FriProof>) -> anyhow::Result<()> {
        let latest_batch_number = self.latest_stored_batch_number().unwrap_or(0);
        anyhow::ensure!(
            value.batch_number() <= latest_batch_number + 1,
            "Attempted to store FRI proofs out of order: previous stored {}, got {}",
            latest_batch_number,
            value.batch_number(),
        );

        if value.batch_number() < latest_batch_number + 1 {
            tracing::warn!(
                "Overriding FRI proof for batch {}. Latest stored batch is {}.",
                value.batch_number(),
                latest_batch_number,
            )
        }

        let key = value.batch_number().to_be_bytes();
        let mut batch: WriteBatch<'_, ProofColumnFamily> = self.db.new_write_batch();
        batch.put_cf(
            ProofColumnFamily::Proofs,
            &key,
            serde_json::to_vec(value)?.as_slice(),
        );
        self.db.write(batch)?;
        Ok(())
    }

    pub fn latest_stored_batch_number(&self) -> Option<u64> {
        let max_key = u64::MAX.to_be_bytes();

        let mut iter = self
            .db
            .to_iterator_cf(ProofColumnFamily::Proofs, ..=&max_key[..]);

        iter.next().map(|(key, _value)| {
            assert_eq!(key.len(), 8);
            u64::from_be_bytes(key[..8].try_into().unwrap())
        })
    }

    /// Loads a BatchWithProof for `batch_number`, if present.
    pub fn get(&self, batch_number: u64) -> anyhow::Result<Option<BatchEnvelope<FriProof>>> {
        let key = batch_number.to_be_bytes();
        let bytes = self.db.get_cf(ProofColumnFamily::Proofs, &key)?;
        let Some(bytes) = bytes else { return Ok(None) };
        let res = serde_json::from_slice(&bytes)?;
        Ok(Some(res))
    }
}
