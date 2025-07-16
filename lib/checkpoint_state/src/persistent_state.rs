use zk_ee::utils::Bytes32;
use zk_os_forward_system::run::StorageWrite;
use zksync_storage::db::NamedColumnFamily;
use zksync_storage::RocksDB;

/// Wrapper for map of storage diffs that are persisted in RocksDB.
///
/// Cheaply clonable / thread safe
#[derive(Debug, Clone)]
pub struct PersistentState {
    /// RocksDB handle - cheap to clone
    pub(crate) rocks: RocksDB<StateCF>,
}

impl PersistentState {
    pub fn new(rocks: RocksDB<StateCF>) -> Self {
        Self { rocks }
    }

    pub fn block_number(&self) -> u64 {
        self.rocks
            .get_cf(StateCF::Meta, StateCF::block_key())
            .unwrap()
            .map(|v| u64::from_be_bytes(v.as_slice().try_into().unwrap()))
            .unwrap_or(0)
    }

    pub fn get_storage_slot(&self, key: Bytes32) -> Option<Bytes32> {
        self.rocks
            .get_cf(StateCF::Storage, key.as_u8_array_ref())
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

    pub fn get_preimage(&self, key: Bytes32) -> Option<Vec<u8>> {
        //let latency = PREIMAGES_METRICS.get[&"total"].start();
        let res = self
            .rocks
            .get_cf(StateCF::Preimage, key.as_u8_array_ref())
            .ok()
            .flatten();
        //latency.observe();
        res
    }

    pub(crate) fn add_block<'a, J>(
        &self,
        block_number: u64,
        storage_diffs: Vec<StorageWrite>,
        new_preimages: J,
    ) where
        J: IntoIterator<Item = (Bytes32, &'a Vec<u8>)>,
    {
        //let latency = PREIMAGES_METRICS.set[&"total"].start();

        let mut batch = self.rocks.new_write_batch();

        batch.put_cf(
            StateCF::Meta,
            StateCF::block_key(),
            block_number.to_be_bytes().as_ref(),
        );

        for (k, v) in new_preimages {
            batch.put_cf(StateCF::Preimage, k.as_u8_array_ref(), v);
        }

        for write in storage_diffs {
            batch.put_cf(
                StateCF::Storage,
                write.key.as_u8_array_ref(),
                write.value.as_u8_array_ref(),
            );
        }

        self.rocks.write(batch).expect("RocksDB write failed");
        //latency.observe();
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum StateCF {
    Storage,
    Preimage,
    Meta,
}

impl NamedColumnFamily for StateCF {
    const DB_NAME: &'static str = "state";
    const ALL: &'static [Self] = &[StateCF::Storage, StateCF::Preimage, StateCF::Meta];

    fn name(&self) -> &'static str {
        match self {
            StateCF::Storage => "storage",
            StateCF::Preimage => "preimage",
            StateCF::Meta => "meta",
        }
    }
}

impl StateCF {
    pub(crate) fn block_key() -> &'static [u8] {
        b"block"
    }
}
