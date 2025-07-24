use std::path::PathBuf;
use zksync_os_l1_sender::commitment::StoredBatchInfo;
use zksync_storage::RocksDB;
use zksync_storage::db::{NamedColumnFamily, WriteBatch};

#[derive(Clone, Copy, Debug)]
enum BatcherColumnFamily {
    /// Stores the next L1 block number to be processed under a fixed key.
    StoredBatchInfo,
}

impl NamedColumnFamily for BatcherColumnFamily {
    const DB_NAME: &'static str = "batcher";
    const ALL: &'static [Self] = &[BatcherColumnFamily::StoredBatchInfo];

    fn name(&self) -> &'static str {
        match self {
            BatcherColumnFamily::StoredBatchInfo => "stored_batch_info",
        }
    }
}
pub struct BatcherRocksDBStorage {
    db: RocksDB<BatcherColumnFamily>,
}

impl BatcherRocksDBStorage {
    pub fn new(rocks_db_path: PathBuf) -> Self {
        let db =
            RocksDB::<BatcherColumnFamily>::new(&rocks_db_path.join(BatcherColumnFamily::DB_NAME))
                .expect("Failed to open Batcher DB")
                .with_sync_writes();
        Self { db }
    }
    pub fn get(&self, batch_number: u64) -> anyhow::Result<Option<StoredBatchInfo>> {
        let key = batch_number.to_be_bytes();
        let data = self.db.get_cf(BatcherColumnFamily::StoredBatchInfo, &key)?;
        let Some(bytes) = data else { return Ok(None) };
        let res = serde_json::from_slice(&bytes)?;

        Ok(res)
    }

    pub fn set(
        &self,
        batch_number: u64,
        stored_batch_info: &StoredBatchInfo,
    ) -> anyhow::Result<()> {
        let mut batch: WriteBatch<'_, BatcherColumnFamily> = self.db.new_write_batch();
        let key = batch_number.to_be_bytes();
        batch.put_cf(
            BatcherColumnFamily::StoredBatchInfo,
            &key,
            serde_json::to_vec(stored_batch_info)?.as_slice(),
        );
        self.db.write(batch)?;
        Ok(())
    }
}
