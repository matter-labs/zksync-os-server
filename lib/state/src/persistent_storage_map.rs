use crate::metrics::STORAGE_MAP_METRICS;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::Instant;
use zksync_os_genesis::Genesis;
use zksync_os_interface::bytes32::Bytes32;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::NamedColumnFamily;

/// Wrapper for map of storage diffs that are persisted in RocksDB.
///
/// Cheaply clonable / thread safe
#[derive(Debug, Clone)]
pub struct PersistentStorageMap {
    /// RocksDB handle for the persistent base - cheap to clone
    pub rocks: RocksDB<StorageMapCF>,

    /// block in rocksDB is no older than
    pub persistent_block_lower_bound: Arc<AtomicU64>,
    /// block in rocksDB is no newer than
    pub persistent_block_upper_bound: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug)]
pub enum StorageMapCF {
    Storage,
    Meta,
}

impl NamedColumnFamily for StorageMapCF {
    const DB_NAME: &'static str = "storage_map";
    const ALL: &'static [Self] = &[StorageMapCF::Storage, StorageMapCF::Meta];

    fn name(&self) -> &'static str {
        match self {
            StorageMapCF::Storage => "storage",
            StorageMapCF::Meta => "meta",
        }
    }
}

impl StorageMapCF {
    fn base_block_key() -> &'static [u8] {
        b"base_block"
    }
}

impl PersistentStorageMap {
    pub fn new(rocks: RocksDB<StorageMapCF>, genesis: &Genesis) -> Self {
        let rocksdb_block_number = rocksdb_block_number(&rocks);
        let this = Self {
            rocks,
            persistent_block_lower_bound: Arc::new(rocksdb_block_number.unwrap_or(0).into()),
            persistent_block_upper_bound: Arc::new(rocksdb_block_number.unwrap_or(0).into()),
        };
        if rocksdb_block_number.is_none() {
            this.compact_sync(
                0,
                genesis.state().storage_logs.clone().into_iter().collect(),
            );
        }
        this
    }

    pub fn compact_sync(&self, new_block_number: u64, diffs: HashMap<Bytes32, Bytes32>) {
        let started_at = Instant::now();

        let (prev_persisted, initial_upper) = (
            self.persistent_block_lower_bound.load(Ordering::Relaxed),
            self.persistent_block_upper_bound.load(Ordering::Relaxed),
        );

        assert_eq!(
            prev_persisted, initial_upper,
            "StorageMap: persistent bounds must be equal when starting compaction, got: {prev_persisted} and {initial_upper}",
        );

        let mut batch = self.rocks.new_write_batch();

        for (k, v) in diffs {
            batch.put_cf(
                StorageMapCF::Storage,
                k.as_u8_array_ref(),
                v.as_u8_array_ref(),
            );
        }
        batch.put_cf(
            StorageMapCF::Meta,
            StorageMapCF::base_block_key(),
            new_block_number.to_be_bytes().as_ref(),
        );

        // This assumes there are no active StorageView with target block below new_block_number
        // (after updating `persistent_block_upper_bound` no new ones will be created, but older may still exist - we should track it separately later)
        //
        // StorageViews with later target blocks, but still referencing diffs that are being compacted are allowed with current implementation,
        // For example, we are compacting until block N, and there is a StorageView with target block N-k. In this case:
        // When traversing diffs backwards, we may look at blocks N-1, N-2, ..., N-k - and then on miss we need to fallback to RocksDB which is already at block N.
        // But that's OK since that's equivalent to just looking through blocks N-1, N-2, ..., N-k again (but without actual iteration since it's already compacted

        // todo: two atomics may be redundant
        self.persistent_block_upper_bound
            .store(new_block_number, Ordering::Relaxed);
        self.rocks.write(batch).expect("RocksDB write failed");
        self.persistent_block_lower_bound
            .store(new_block_number, Ordering::Relaxed);

        STORAGE_MAP_METRICS
            .compact
            .observe(started_at.elapsed() / (new_block_number - prev_persisted).max(1) as u32);
        STORAGE_MAP_METRICS
            .compact_batch_size
            .observe(new_block_number - prev_persisted);
    }

    pub fn rocksdb_block_number(&self) -> u64 {
        rocksdb_block_number(&self.rocks).unwrap()
    }

    pub fn persistent_block_upper_bound(&self) -> u64 {
        self.persistent_block_upper_bound.load(Ordering::Relaxed)
    }

    pub fn get(&self, key: Bytes32) -> Option<Bytes32> {
        self.rocks
            .get_cf(StorageMapCF::Storage, key.as_u8_array_ref())
            .ok()
            .flatten()
            .map(|bytes| {
                let arr: [u8; 32] = bytes
                    .as_slice()
                    .try_into() // Vec<u8> → [u8; 32]
                    .expect("value must be 32 bytes");
                Bytes32::from(arr)
            })
    }
}

fn rocksdb_block_number(rocks_db: &RocksDB<StorageMapCF>) -> Option<u64> {
    rocks_db
        .get_cf(StorageMapCF::Meta, StorageMapCF::base_block_key())
        .unwrap()
        .map(|v| u64::from_be_bytes(v.as_slice().try_into().unwrap()))
}
