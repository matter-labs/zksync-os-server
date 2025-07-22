//! RocksDB‑backed persistence for **FRI** and **SNARK** proofs.
//!
//! • **FRI proofs**
//!     * Column‑family  : `fri_proofs`
//!     * Key            : `block_number` encoded **big‑endian** (`u64::to_be_bytes`)
//!     * Value          : raw proof bytes
//!
//! • **SNARK proofs**
//!     * Column‑family  : `snark_proofs`
//!     * Key            : **`from_block` big‑endian**
//!     * Value          : `to_block` (8 bytes, big‑endian) **followed by** raw proof bytes
//!
//!   Storing `to_block` inside the value lets us obtain the latest SNARK range
//!   with a single `SeekToLast` (no full iteration).

use zksync_storage::db::{NamedColumnFamily, WriteBatch};
use zksync_storage::RocksDB;

/// Column‑families used by the proof store.
#[derive(Copy, Clone, Debug)]
pub enum ProofColumnFamily {
    /// `block_number (u64, BE)` → raw FRI proof bytes
    FriProofs,
    /// `from_block (u64, BE)` → `[to_block (u64, BE) || snark bytes]`
    SnarkProofs,
}

impl NamedColumnFamily for ProofColumnFamily {
    const DB_NAME: &'static str = "proof_storage";
    const ALL: &'static [Self] = &[Self::FriProofs, Self::SnarkProofs];

    fn name(&self) -> &'static str {
        match self {
            ProofColumnFamily::FriProofs => "fri_proofs",
            ProofColumnFamily::SnarkProofs => "snark_proofs",
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

    // ──────────────── FRI ────────────────

    pub fn save_fri_proof(&self, block_number: u64, proof: &[u8]) -> anyhow::Result<()> {
        let key = block_number.to_be_bytes();
        let mut batch: WriteBatch<'_, _> = self.db.new_write_batch();
        batch.put_cf(ProofColumnFamily::FriProofs, &key, proof);
        self.db.write(batch)?;
        Ok(())
    }

    pub fn save_fri_proof_with_prover(
        &self,
        block_number: u64,
        proof: &[u8],
        prover_id: &str,
    ) -> anyhow::Result<()> {
        tracing::info!(block_number, prover_id, "Saving FRI proof with prover ID");
        self.save_fri_proof(block_number, proof)
    }

    pub fn get_fri_proof(&self, block_number: u64) -> Option<Vec<u8>> {
        let key = block_number.to_be_bytes();
        self.db
            .get_cf(ProofColumnFamily::FriProofs, &key)
            .expect("DB read failure")
    }

    // ──────────────── SNARK ────────────────

    /// Persist a SNARK proof for the block‑range `[from_block, to_block]`.
    /// Ranges **must be adjacent**; i.e. `from_block` must equal
    /// `latest_snark_range().map(|(_,to)| to + 1).unwrap_or(1)`.
    pub fn save_snark_proof(
        &self,
        from_block: u64,
        to_block: u64,
        proof: &[u8],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(from_block <= to_block, "invalid SNARK range");

        // Enforce adjacency.
        let expected = self.next_expected_snark_range_start();
        anyhow::ensure!(
            from_block == expected,
            "SNARK ranges must be adjacent (expected start {}, got {})",
            expected,
            from_block
        );

        // Key → from_block (BE)
        let key = from_block.to_be_bytes();

        // Value → [to_block (BE) || proof bytes]
        let mut value = Vec::with_capacity(8 + proof.len());
        value.extend_from_slice(&to_block.to_be_bytes());
        value.extend_from_slice(proof);

        let mut batch: WriteBatch<'_, _> = self.db.new_write_batch();
        batch.put_cf(ProofColumnFamily::SnarkProofs, &key, &value);
        self.db.write(batch)?;
        Ok(())
    }

    /// Load a SNARK proof that starts from `from_block`,
    /// returning the tuple (to_block, raw proof bytes).
    pub fn get_snark_proof(&self, from_block: u64) -> Option<(u64, Vec<u8>)> {
        let key = from_block.to_be_bytes();
        let value = self
            .db
            .get_cf(ProofColumnFamily::SnarkProofs, &key)
            .expect("DB read failure")?;

        assert!(value.len() >= 8);

        let stored_to = u64::from_be_bytes(value[..8].try_into().ok()?);
        let proof = value[8..].to_vec();
        Some((stored_to, proof))
    }

    /// Returns the SNARK range with the **largest `from_block`** (i.e. the
    /// latest range) or `None` if no SNARK proofs are stored.
    pub fn latest_snark_range(&self) -> Option<(u64, u64)> {
        let max_key = u64::MAX.to_be_bytes();

        let mut iter = self
            .db
            .to_iterator_cf(ProofColumnFamily::SnarkProofs, ..=&max_key[..]);

        iter.next().and_then(|(key, value)| {
            assert_eq!(key.len(), 8);
            assert!(value.len() >= 8);
            let from = u64::from_be_bytes(key[..8].try_into().ok()?);
            let to = u64::from_be_bytes(value[..8].try_into().ok()?);
            Some((from, to))
        })
    }

    /// The first block that still needs a SNARK proof.
    pub fn next_expected_snark_range_start(&self) -> u64 {
        self.latest_snark_range().map(|(_, to)| to + 1).unwrap_or(1)
    }
    pub fn estimate_number_of_fri_proofs(&self) -> u64 {
        self.db.estimated_number_of_entries(ProofColumnFamily::FriProofs)
    }

    pub fn estimate_number_of_snark_proofs(&self) -> u64 {
        self.db.estimated_number_of_entries(ProofColumnFamily::SnarkProofs)
    }

    /// Diagnostic helper: returns **all** stored SNARK ranges.
    #[allow(dead_code)]
    pub fn get_snark_ranges(&self) -> Vec<(u64, u64)> {
        self.db
            .prefix_iterator_cf(ProofColumnFamily::SnarkProofs, &[])
            .filter_map(|(key, value)| {
                if key.len() != 8 || value.len() < 8 {
                    return None;
                }
                let from = u64::from_be_bytes(key[..8].try_into().ok()?);
                let to = u64::from_be_bytes(value[..8].try_into().ok()?);
                Some((from, to))
            })
            .collect()
    }

    /// Diagnostic helper: returns **all** stored FRI proofs.
    #[allow(dead_code)]
    pub fn get_blocks_with_fri_proof(&self) -> Vec<u64> {
        self.db
            .prefix_iterator_cf(ProofColumnFamily::FriProofs, &[])
            .map(|(key, _)| {
                let arr: [u8; 8] = key.as_ref()[..8].try_into().expect("key length");
                u64::from_be_bytes(arr)
            })
            .collect()
    }
}
