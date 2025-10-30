use alloy::primitives::B256;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use zksync_os_interface::types::StorageWrite;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::NamedColumnFamily;

#[derive(Clone, Copy, Debug)]
pub enum StorageCF {
    Data,
    Meta,
}

impl NamedColumnFamily for StorageCF {
    const DB_NAME: &'static str = "state_full_diffs";
    const ALL: &'static [Self] = &[StorageCF::Data, StorageCF::Meta];

    fn name(&self) -> &'static str {
        match self {
            StorageCF::Data => "data",
            StorageCF::Meta => "meta",
        }
    }
}

impl StorageCF {
    fn latest_block_key() -> &'static [u8] {
        b"latest_block"
    }
}

#[derive(Debug, Clone)]
pub struct FullDiffsStorage {
    rocks: RocksDB<StorageCF>,
    latest_block: Arc<AtomicU64>,
}

// Builds the composite end key for reverse iteration: hashed_key || block_number_be
// Keys are ordered lexicographically in RocksDB. Since block_number is stored big-endian,
// for a fixed 32-byte hashed_key prefix, all versions are contiguous and ordered by block.
// Thus, iterating in reverse starting from (key || block) will yield at most one relevant
// entry for our key: the latest write at or before the requested block.
impl FullDiffsStorage {
    pub fn new(path: &Path) -> anyhow::Result<Self> {
        let rocks = RocksDB::<StorageCF>::new(path)?;
        let latest_block = rocks
            .get_cf(StorageCF::Meta, StorageCF::latest_block_key())
            .ok()
            .flatten()
            .map(|v| u64::from_be_bytes(v.as_slice().try_into().unwrap()))
            .unwrap_or(0);
        tracing::info!(latest_block, "initialized full diffs storage");
        Ok(Self {
            rocks,
            latest_block: Arc::new(AtomicU64::new(latest_block)),
        })
    }

    pub fn latest_block(&self) -> u64 {
        self.latest_block.load(Ordering::Relaxed)
    }

    pub fn add_block(
        &self,
        block_number: u64,
        writes: Vec<StorageWrite>,
        override_allowed: bool,
    ) -> anyhow::Result<()> {
        let mut latest_block = self.latest_block();

        if override_allowed && block_number <= latest_block {
            tracing::info!(
                "Rolling back state for block range [{}; {}]",
                block_number,
                latest_block
            );
            let mut batch = self.rocks.new_write_batch();
            // Iterate through all keys and delete those with block_number >= the given block_number
            for (k, _v) in self.rocks.prefix_iterator_cf(StorageCF::Data, &[]) {
                let key_block_number = u64::from_be_bytes(k[32..40].try_into()?);
                if key_block_number >= block_number {
                    batch.delete_cf(StorageCF::Data, &k);
                }
            }
            self.rocks.write(batch)?;
            latest_block = block_number.saturating_sub(1);
        }
        // We cannot do validation for genesis block because there is currently no way to distinguish between
        // initialized empty storage and initialized storage with just genesis (both have latest block
        // equal to 0).
        // todo: distinguish between empty state and state with just genesis
        if !override_allowed && block_number != 0 {
            if block_number <= latest_block {
                for write in writes {
                    let expected_value = self.read_at(block_number, write.key).unwrap_or_default();
                    assert_eq!(
                        expected_value, write.value,
                        "historical write discrepancy for key={} at block_number={}",
                        write.key, block_number
                    );
                }
                return Ok(());
            }
            assert_eq!(
                block_number,
                latest_block + 1,
                "StorageMap: attempt to add block number {} - previous block is {}. Cannot have gaps in block data",
                block_number,
                latest_block + 1
            );
        }

        let per_key: HashMap<B256, B256> = writes.into_iter().map(|w| (w.key, w.value)).collect();

        let mut batch = self.rocks.new_write_batch();
        for (k, v) in per_key.into_iter() {
            let key = Self::key_for_storage_write(&block_number, k);
            batch.put_cf(StorageCF::Data, &key, v.as_slice());
        }
        batch.put_cf(
            StorageCF::Meta,
            StorageCF::latest_block_key(),
            block_number.to_be_bytes().as_ref(),
        );
        self.rocks.write(batch)?;
        self.latest_block.store(block_number, Ordering::Relaxed);
        Ok(())
    }

    pub fn read_at(&self, block_number: u64, key: B256) -> Option<B256> {
        if block_number > self.latest_block() {
            return None;
        }
        let end = Self::key_for_storage_write(&block_number, key);

        let mut iter = self
            .rocks
            .to_iterator_cf(StorageCF::Data, ..=end.as_slice());

        if let Some((k, v)) = iter.next() {
            assert_eq!(
                k.len(),
                40,
                "FullDiffsStorage: unexpected key length in Data CF; expected 40 bytes"
            );
            // If the very first item has a different prefix,
            // it means there are no writes for this key <= block and we
            // can return None immediately.
            if &k[..32] != key.as_slice() {
                return None;
            }
            let arr: [u8; 32] = v.as_ref().try_into().ok()?;
            return Some(B256::from(arr));
        }
        None
    }

    fn key_for_storage_write(block_number: &u64, k: B256) -> Vec<u8> {
        let mut key = Vec::with_capacity(40);
        key.extend_from_slice(k.as_slice());
        key.extend_from_slice(&block_number.to_be_bytes());
        key
    }
}
