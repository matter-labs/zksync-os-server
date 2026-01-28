use alloy::primitives::BlockNumber;
use anyhow::Context;
use std::path::Path;
use zksync_os_batch_types::DiscoveredCommittedBatch;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::{NamedColumnFamily, WriteBatch as RocksdbWriteBatch};
use zksync_os_storage_api::{ReadBatch, WriteBatch};

#[derive(Clone, Debug)]
pub struct ExecutedBatchStorage {
    db: RocksDB<ExecutedBatchColumnFamily>,
}

/// Column families for storage of executed batches.
#[derive(Copy, Clone, Debug)]
pub enum ExecutedBatchColumnFamily {
    /// batch_number (be) => DiscoveredCommittedBatch (JSON)
    BatchInfo,
    /// block_number (be) => batch number which block range starts with this block (be)
    FirstBlockIndex,
    /// Stores the latest appended batch number under a fixed key.
    Latest,
}

impl NamedColumnFamily for ExecutedBatchColumnFamily {
    const DB_NAME: &'static str = "executed_batch_storage";
    const ALL: &'static [Self] = &[
        ExecutedBatchColumnFamily::BatchInfo,
        ExecutedBatchColumnFamily::FirstBlockIndex,
        ExecutedBatchColumnFamily::Latest,
    ];

    fn name(&self) -> &'static str {
        match self {
            ExecutedBatchColumnFamily::BatchInfo => "batch_info",
            ExecutedBatchColumnFamily::FirstBlockIndex => "first_block_index",
            ExecutedBatchColumnFamily::Latest => "latest",
        }
    }
}

impl ExecutedBatchStorage {
    /// Key under `Latest` CF for tracking the highest batch number.
    const LATEST_KEY: &'static [u8] = b"latest_batch";

    pub fn new(db_path: &Path) -> Self {
        let db = RocksDB::<ExecutedBatchColumnFamily>::new(db_path)
            .expect("Failed to open ExecutedBatchStorage");

        Self { db }
    }

    fn write_batch_unchecked(&self, executed_batch: DiscoveredCommittedBatch) {
        let batch_number_key = executed_batch.number().to_be_bytes().to_vec();
        let first_block_number_key = executed_batch.first_block_number().to_be_bytes().to_vec();
        let batch_info_value = serde_json::to_vec(&executed_batch)
            .expect("failed to serialize DiscoveredCommittedBatch");
        let mut batch: RocksdbWriteBatch<'_, ExecutedBatchColumnFamily> = self.db.new_write_batch();
        batch.put_cf(
            ExecutedBatchColumnFamily::Latest,
            Self::LATEST_KEY,
            &batch_number_key,
        );
        batch.put_cf(
            ExecutedBatchColumnFamily::BatchInfo,
            &batch_number_key,
            &batch_info_value,
        );
        batch.put_cf(
            ExecutedBatchColumnFamily::FirstBlockIndex,
            &first_block_number_key,
            &batch_number_key,
        );
        self.db
            .write(batch)
            .expect("failed to write to batch storage");
    }
}

impl ReadBatch for ExecutedBatchStorage {
    fn get_batch_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> anyhow::Result<Option<DiscoveredCommittedBatch>> {
        let block_key = block_number.to_be_bytes();

        let mut iter = self.db.to_iterator_cf(
            ExecutedBatchColumnFamily::FirstBlockIndex,
            ..=block_key.as_slice(),
        );
        if let Some((_, v)) = iter.next() {
            let arr: [u8; 8] = v.as_ref().try_into().context("invalid first block index")?;
            let batch_number = u64::from_be_bytes(arr);
            let batch = self
                .get_batch_by_number(batch_number)?
                .expect("batch indexed in FirstBlockIndex not found in DB");
            if !batch.block_range.contains(&block_number) {
                // This can be hit if requested block number is farther than latest persisted block
                // number.
                return Ok(None);
            }
            Ok(Some(batch))
        } else {
            Ok(None)
        }
    }

    fn get_batch_by_number(
        &self,
        batch_number: u64,
    ) -> anyhow::Result<Option<DiscoveredCommittedBatch>> {
        let batch_key = batch_number.to_be_bytes();
        let Some(bytes) = self
            .db
            .get_cf(ExecutedBatchColumnFamily::BatchInfo, &batch_key)
            .context("cannot read from DB")?
        else {
            return Ok(None);
        };

        serde_json::from_slice(&bytes).context("failed to deserialize context")
    }

    fn latest_batch(&self) -> u64 {
        self.db
            .get_cf(ExecutedBatchColumnFamily::Latest, Self::LATEST_KEY)
            .expect("cannot read from DB")
            .map(|bytes| {
                assert_eq!(bytes.len(), 8);
                let arr: [u8; 8] = bytes.as_slice().try_into().unwrap();
                u64::from_be_bytes(arr)
            })
            .unwrap_or_default()
    }
}

impl WriteBatch for ExecutedBatchStorage {
    fn write(&self, batch: DiscoveredCommittedBatch) {
        self.write_batch_unchecked(batch)
    }
}
