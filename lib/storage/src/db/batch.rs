use crate::metrics::BATCH_STORAGE_METRICS;
use alloy::primitives::BlockNumber;
use anyhow::Context;
use std::path::Path;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::{NamedColumnFamily, WriteBatch as RocksdbWriteBatch};
use zksync_os_storage_api::{PersistedBatch, ReadBatch, WriteBatch};

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

        // todo: initialize with genesis
        Self { db }
    }

    fn write_batch_unchecked(&self, executed_batch: PersistedBatch) {
        let persist_latency_observer = BATCH_STORAGE_METRICS.persist_latency.start();
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
        BATCH_STORAGE_METRICS
            .data_size
            .observe(batch.size_in_bytes());
        self.db
            .write(batch)
            .expect("failed to write to batch storage");
        persist_latency_observer.observe();
        BATCH_STORAGE_METRICS
            .persist_batch_number
            .set(executed_batch.number());
    }

    /// Removes persisted batch metadata for all batches whose L2 block range intersects
    /// `from_block..`.
    pub fn rollback_from_l2_block(&self, from_block: BlockNumber) -> anyhow::Result<()> {
        let latest_batch = self.latest_batch();
        if latest_batch == 0 {
            return Ok(());
        }

        let first_batch_to_delete =
            if let Some(batch) = self.get_batch_by_block_number(from_block)? {
                Some(batch.number())
            } else {
                let start_key = from_block.to_be_bytes();
                self.db
                    .from_iterator_cf(
                        ExecutedBatchColumnFamily::FirstBlockIndex,
                        start_key.as_slice()..,
                    )
                    .next()
                    .map(|(_, batch_number_bytes)| {
                        let arr: [u8; 8] = batch_number_bytes
                            .as_ref()
                            .try_into()
                            .context("invalid first block index")?;
                        anyhow::Ok(u64::from_be_bytes(arr))
                    })
                    .transpose()?
            };

        let Some(first_batch_to_delete) = first_batch_to_delete else {
            return Ok(());
        };
        if first_batch_to_delete > latest_batch {
            return Ok(());
        }

        let mut batches_to_delete = vec![];
        for batch_number in first_batch_to_delete..=latest_batch {
            let batch = self.get_batch_by_number(batch_number)?;
            batches_to_delete.push((batch_number, batch.map(|batch| batch.first_block_number())));
        }

        let mut latest_batch_to_keep = first_batch_to_delete.saturating_sub(1);
        while latest_batch_to_keep > 0 && self.get_batch_by_number(latest_batch_to_keep)?.is_none()
        {
            latest_batch_to_keep -= 1;
        }

        tracing::warn!(
            from_block,
            first_batch_to_delete,
            latest_batch,
            latest_batch_to_keep,
            "rolling back executed batch storage"
        );
        let mut batch = self.db.new_write_batch();
        for (batch_number, first_block_number) in batches_to_delete {
            batch.delete_cf(
                ExecutedBatchColumnFamily::BatchInfo,
                &batch_number.to_be_bytes(),
            );
            if let Some(first_block_number) = first_block_number {
                batch.delete_cf(
                    ExecutedBatchColumnFamily::FirstBlockIndex,
                    &first_block_number.to_be_bytes(),
                );
            }
        }
        if latest_batch_to_keep == 0 {
            batch.delete_cf(ExecutedBatchColumnFamily::Latest, Self::LATEST_KEY);
        } else {
            batch.put_cf(
                ExecutedBatchColumnFamily::Latest,
                Self::LATEST_KEY,
                &latest_batch_to_keep.to_be_bytes(),
            );
        }
        self.db.write(batch)?;
        Ok(())
    }
}

impl ReadBatch for ExecutedBatchStorage {
    fn get_batch_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> anyhow::Result<Option<PersistedBatch>> {
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

    fn get_batch_by_number(&self, batch_number: u64) -> anyhow::Result<Option<PersistedBatch>> {
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
    fn write(&self, batch: PersistedBatch) {
        self.write_batch_unchecked(batch)
    }
}
