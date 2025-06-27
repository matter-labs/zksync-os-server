use crate::commitment::StoredBatchInfo;
use std::path::PathBuf;
use zksync_storage::db::{NamedColumnFamily, WriteBatch};
use zksync_storage::RocksDB;

const L1_SENDER_DB_NAME: &str = "l1_sender";

// TODO: Remove, L1 sender should not have persistence
#[derive(Clone, Debug)]
pub struct L1SenderRocksdbStorage {
    db: RocksDB<L1SenderColumnFamily>,
}

#[derive(Clone, Copy, Debug)]
enum L1SenderColumnFamily {
    LastCommittedBatch,
}

impl NamedColumnFamily for L1SenderColumnFamily {
    const DB_NAME: &'static str = "l1_sender";
    const ALL: &'static [Self] = &[L1SenderColumnFamily::LastCommittedBatch];

    fn name(&self) -> &'static str {
        match self {
            L1SenderColumnFamily::LastCommittedBatch => "last_committed_batch",
        }
    }
}

impl L1SenderRocksdbStorage {
    /// Key under [`L1SenderColumnFamily::LastCommittedBatch`] CF for tracking the last committed batch.
    const LAST_COMMITTED_BATCH_KEY: &'static [u8] = b"last_committed_batch";

    pub fn new(rocks_db_path: PathBuf) -> Self {
        let db = RocksDB::<L1SenderColumnFamily>::new(&rocks_db_path.join(L1_SENDER_DB_NAME))
            .expect("Failed to open L1Sender DB")
            .with_sync_writes();
        Self { db }
    }

    pub fn get_last_committed_batch(&self) -> Option<StoredBatchInfo> {
        let data = self
            .db
            .get_cf(
                L1SenderColumnFamily::LastCommittedBatch,
                Self::LAST_COMMITTED_BATCH_KEY,
            )
            .expect("Cannot read from DB")?;
        serde_json::from_slice(&data).expect("L1 last committed batch malformed")
    }

    pub fn set_last_committed_batch(&self, last_committed_batch: &StoredBatchInfo) {
        let mut batch: WriteBatch<'_, L1SenderColumnFamily> = self.db.new_write_batch();
        batch.put_cf(
            L1SenderColumnFamily::LastCommittedBatch,
            Self::LAST_COMMITTED_BATCH_KEY,
            serde_json::to_vec(last_committed_batch).unwrap().as_slice(),
        );
        self.db.write(batch).expect("Failed to write to DB");
    }
}
