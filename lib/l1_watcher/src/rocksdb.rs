use alloy::primitives::BlockNumber;
use std::path::PathBuf;
use zksync_storage::db::{NamedColumnFamily, WriteBatch};
use zksync_storage::RocksDB;

const L1_WATCHER_DB_NAME: &str = "l1_watcher";

// TODO: Make persistence optional by refactoring storage into a trait
#[derive(Clone, Debug)]
pub struct L1WatcherRocksdbStorage {
    db: RocksDB<L1WatcherColumnFamily>,
}

#[derive(Clone, Copy, Debug)]
enum L1WatcherColumnFamily {
    /// Stores the next L1 block number to be processed under a fixed key.
    NextL1Block,
}

impl NamedColumnFamily for L1WatcherColumnFamily {
    const DB_NAME: &'static str = "l1_watcher";
    const ALL: &'static [Self] = &[L1WatcherColumnFamily::NextL1Block];

    fn name(&self) -> &'static str {
        match self {
            L1WatcherColumnFamily::NextL1Block => "next",
        }
    }
}

impl L1WatcherRocksdbStorage {
    /// Key under [`L1WatcherColumnFamily::NextL1Block`] CF for tracking the next L1 block to process.
    const NEXT_L1_BLOCK_KEY: &'static [u8] = b"next_l1_block";

    pub fn new(rocks_db_path: PathBuf) -> Self {
        let db = RocksDB::<L1WatcherColumnFamily>::new(&rocks_db_path.join(L1_WATCHER_DB_NAME))
            .expect("Failed to open L1Watcher DB")
            .with_sync_writes();
        Self { db }
    }

    /// Sets next L1 block number to be processed.
    pub fn set_next_l1_block(&self, next_l1_block: BlockNumber) {
        let mut batch: WriteBatch<'_, L1WatcherColumnFamily> = self.db.new_write_batch();
        batch.put_cf(
            L1WatcherColumnFamily::NextL1Block,
            Self::NEXT_L1_BLOCK_KEY,
            &next_l1_block.to_be_bytes(),
        );
        self.db.write(batch).expect("Failed to write to DB");
    }

    /// Returns the next L1 block number that is yet to be processed, or `None` if empty.
    pub fn next_l1_block(&self) -> Option<BlockNumber> {
        self.db
            .get_cf(L1WatcherColumnFamily::NextL1Block, Self::NEXT_L1_BLOCK_KEY)
            .expect("Cannot read from DB")
            .map(|bytes| {
                assert_eq!(bytes.len(), 8);
                let arr: [u8; 8] = bytes.as_slice().try_into().unwrap();
                BlockNumber::from_be_bytes(arr)
            })
    }
}
