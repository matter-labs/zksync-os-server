use crate::storage::BuildFoldHasher;
use alloy::primitives::B256;
use dashmap::DashMap;
use std::path::Path;
use std::sync::Arc;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::NamedColumnFamily;

#[derive(Clone, Copy, Debug)]
pub enum PreimagesCF {
    Storage,
}

impl NamedColumnFamily for PreimagesCF {
    const DB_NAME: &'static str = "preimages_full_diffs";
    const ALL: &'static [Self] = &[PreimagesCF::Storage];

    fn name(&self) -> &'static str {
        match self {
            PreimagesCF::Storage => "storage",
        }
    }
}

#[derive(Clone, Debug)]
enum Backend {
    Rocks(RocksDB<PreimagesCF>),
    /// Bench-only: preimages are content-addressed (hash -> bytes), so a plain concurrent map
    /// with no versioning matches the RocksDB semantics.
    InMemory(Arc<DashMap<B256, Vec<u8>, BuildFoldHasher>>),
}

#[derive(Clone, Debug)]
pub struct FullDiffsPreimages {
    backend: Backend,
}

impl FullDiffsPreimages {
    pub fn new(path: &Path) -> anyhow::Result<Self> {
        let rocks = RocksDB::<PreimagesCF>::new(path)?;
        Ok(Self {
            backend: Backend::Rocks(rocks),
        })
    }

    pub fn new_in_memory() -> Self {
        Self {
            backend: Backend::InMemory(Arc::new(DashMap::default())),
        }
    }

    pub fn get(&self, key: B256) -> Option<Vec<u8>> {
        match &self.backend {
            Backend::Rocks(rocks) => rocks
                .get_cf(PreimagesCF::Storage, key.as_slice())
                .ok()
                .flatten(),
            Backend::InMemory(map) => map.get(&key).map(|v| v.clone()),
        }
    }

    pub fn add<'a, J>(&self, diffs: J) -> anyhow::Result<()>
    where
        J: IntoIterator<Item = (B256, &'a Vec<u8>)>,
    {
        match &self.backend {
            Backend::Rocks(rocks) => {
                let mut batch = rocks.new_write_batch();
                for (k, v) in diffs.into_iter() {
                    batch.put_cf(PreimagesCF::Storage, k.as_slice(), v);
                }
                rocks.write(batch)?;
            }
            Backend::InMemory(map) => {
                for (k, v) in diffs.into_iter() {
                    map.insert(k, v.clone());
                }
            }
        }
        Ok(())
    }
}
