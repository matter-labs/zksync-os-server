use crate::metrics::STORAGE_MAP_METRICS;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::time::Instant;
use zk_ee::utils::Bytes32;
use zksync_storage::db::NamedColumnFamily;
use zksync_storage::RocksDB;

/// Wrapper for map of storage diffs that are persisted in RocksDB.
///
/// Cheaply clonable / thread safe
#[derive(Debug, Clone)]
pub struct PersistentStorageMap {
    /// RocksDB handle for the persistent base - cheap to clone
    pub rocks: RocksDB<StorageMapCF>,
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
    pub fn new(rocks: RocksDB<StorageMapCF>) -> Self {
        Self { rocks }
    }

    pub fn rocksdb_block_number(&self) -> u64 {
        self.rocks
            .get_cf(StorageMapCF::Meta, StorageMapCF::base_block_key())
            .unwrap()
            .map(|v| u64::from_be_bytes(v.as_slice().try_into().unwrap()))
            .unwrap_or(0)
    }

    pub fn get(&self, key: Bytes32) -> Option<Bytes32> {
        let res = self
            .rocks
            .get_cf(StorageMapCF::Storage, key.as_u8_array_ref())
            .ok()
            .flatten()
            .map(|bytes| {
                let arr: [u8; 32] = bytes
                    .as_slice()
                    .try_into() // Vec<u8> → [u8; 32]
                    .expect("value must be 32 bytes");
                Bytes32::from(arr)
            });
        res
    }
}
