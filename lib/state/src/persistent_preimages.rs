use crate::metrics::PREIMAGES_METRICS;
use zk_ee::utils::Bytes32;
use zk_os_forward_system::run::PreimageSource;
use zksync_os_genesis::Genesis;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::NamedColumnFamily;

#[derive(Clone, Debug)]
pub struct PersistentPreimages {
    /// RocksDB handle for the persistent base - cheap to clone
    pub rocks: RocksDB<PreimagesCF>,
}

#[derive(Clone, Copy, Debug)]
pub enum PreimagesCF {
    Storage,
    Meta,
}

impl NamedColumnFamily for PreimagesCF {
    const DB_NAME: &'static str = "preimages";
    const ALL: &'static [Self] = &[PreimagesCF::Storage, PreimagesCF::Meta];

    fn name(&self) -> &'static str {
        match self {
            PreimagesCF::Storage => "storage",
            PreimagesCF::Meta => "meta",
        }
    }
}

impl PreimagesCF {
    pub fn block_key() -> &'static [u8] {
        b"block"
    }
}

impl PersistentPreimages {
    pub async fn new(rocks: RocksDB<PreimagesCF>, genesis: &Genesis) -> Self {
        let genesis_needed = rocksdb_block_number(&rocks).is_none();
        let this = Self { rocks };
        if genesis_needed {
            let force_deploy_preimages = genesis.genesis_upgrade_tx().await.1;
            let iter = genesis
                .state()
                .preimages
                .iter()
                .chain(force_deploy_preimages.iter())
                .map(|(k, v)| (*k, v));
            this.add(0, iter);
        }

        this
    }

    pub fn rocksdb_block_number(&self) -> u64 {
        rocksdb_block_number(&self.rocks).unwrap()
    }

    /// Insert multiple preimages at once.
    ///
    /// Each `(key, preimage)` is added if the key is not already present.
    /// This batch insertion is safe for concurrent use.
    pub fn get(&self, key: Bytes32) -> Option<Vec<u8>> {
        let latency_observer = PREIMAGES_METRICS.get[&"total"].start();
        let res = self
            .rocks
            .get_cf(PreimagesCF::Storage, key.as_u8_array_ref())
            .ok()
            .flatten();
        latency_observer.observe();
        res
    }

    pub fn add<'a, J>(&self, new_block_number: u64, diffs: J)
    where
        J: IntoIterator<Item = (Bytes32, &'a Vec<u8>)>,
    {
        let latency_observer = PREIMAGES_METRICS.set[&"total"].start();

        let mut batch = self.rocks.new_write_batch();

        for (k, v) in diffs {
            batch.put_cf(PreimagesCF::Storage, k.as_u8_array_ref(), v);
        }
        batch.put_cf(
            PreimagesCF::Meta,
            PreimagesCF::block_key(),
            new_block_number.to_be_bytes().as_ref(),
        );

        self.rocks.write(batch).expect("RocksDB write failed");
        latency_observer.observe();
    }
}

impl PreimageSource for PersistentPreimages {
    fn get_preimage(&mut self, hash: Bytes32) -> Option<Vec<u8>> {
        self.get(hash)
    }
}

fn rocksdb_block_number(rocks_db: &RocksDB<PreimagesCF>) -> Option<u64> {
    rocks_db
        .get_cf(PreimagesCF::Meta, PreimagesCF::block_key())
        .ok()
        .flatten()
        .map(|v| u64::from_be_bytes(v.as_slice().try_into().unwrap()))
}
