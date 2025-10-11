use std::path::Path;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::{NamedColumnFamily, WriteBatch};

const BATCH_INDEX_DB_NAME: &str = "batch_index";

#[derive(Copy, Clone, Debug)]
enum BatchIndexCF {
    BlockToBatch,
    BatchRanges,
    Meta,
}

impl BatchIndexCF {
    fn latest_batch_key() -> &'static [u8] {
        b"latest_batch"
    }
}

impl NamedColumnFamily for BatchIndexCF {
    const DB_NAME: &'static str = BATCH_INDEX_DB_NAME;
    const ALL: &'static [Self] = &[Self::BlockToBatch, Self::BatchRanges, Self::Meta];

    fn name(&self) -> &'static str {
        match self {
            Self::BlockToBatch => "block_to_batch",
            Self::BatchRanges => "batch_ranges",
            Self::Meta => "meta",
        }
    }
}

#[derive(Clone, Debug)]
pub struct BatchIndex {
    rocks: RocksDB<BatchIndexCF>,
}

impl BatchIndex {
    pub fn new(path: &Path) -> Self {
        let rocks = RocksDB::<BatchIndexCF>::new(path).expect("Failed to open batch index DB");
        Self { rocks }
    }

    pub fn get_batch_by_block(&self, block_number: u64) -> Option<u64> {
        // Seek to the greatest first_block key <= provided block using reverse iterator.
        let mut iter = self.rocks.to_iterator_cf(
            BatchIndexCF::BlockToBatch,
            ..=block_number.to_be_bytes().as_slice(),
        );

        if let Some((_, batch_be)) = iter.next() {
            let batch_number = u64::from_be_bytes(batch_be.as_ref().try_into().unwrap());

            // Validate that block is within the stored range for this batch
            if let Some((first, last)) = self.get_batch_range(batch_number)
                && first <= block_number
                && block_number <= last
            {
                return Some(batch_number);
            }
        }

        None
    }

    pub fn get_batch_range(&self, batch: u64) -> Option<(u64, u64)> {
        self.rocks
            .get_cf(BatchIndexCF::BatchRanges, &batch.to_be_bytes())
            .ok()
            .flatten()
            .map(|v| {
                let (a, b) = v.split_at(8);
                let first_block = u64::from_be_bytes(a.try_into().unwrap());
                let last_block = u64::from_be_bytes(b.try_into().unwrap());
                (first_block, last_block)
            })
    }

    pub fn get_last_stored_batch(&self) -> u64 {
        self.rocks
            .get_cf(BatchIndexCF::Meta, BatchIndexCF::latest_batch_key())
            .ok()
            .flatten()
            .map(|v| u64::from_be_bytes(v.try_into().unwrap()))
            .unwrap_or(0) // if no batches stored - assume genesis
    }

    pub fn write_batch_mapping(
        &self,
        batch_number: u64,
        first_block: u64,
        last_block: u64,
    ) -> anyhow::Result<()> {
        // Don't allow adding batches with gaps or out of order.
        if batch_number > self.get_last_stored_batch() + 1 {
            anyhow::bail!(
                "Batches must be added in order without gaps. Last stored batch is {}, attempted to add batch {}",
                self.get_last_stored_batch(),
                batch_number
            );
        }

        // Store a single entry keyed by first_block under BlockToBatch
        // and the range under BatchRanges.
        let mut wb: WriteBatch<'_, BatchIndexCF> = self.rocks.new_write_batch();

        wb.put_cf(
            BatchIndexCF::BlockToBatch,
            &first_block.to_be_bytes(),
            &batch_number.to_be_bytes(),
        );

        let mut range_bytes = [0u8; 16];
        range_bytes[..8].copy_from_slice(&first_block.to_be_bytes());
        range_bytes[8..].copy_from_slice(&last_block.to_be_bytes());
        wb.put_cf(
            BatchIndexCF::BatchRanges,
            &batch_number.to_be_bytes(),
            &range_bytes,
        );
        wb.put_cf(
            BatchIndexCF::Meta,
            BatchIndexCF::latest_batch_key(),
            &batch_number.to_be_bytes(),
        );

        self.rocks.write(wb)?;

        Ok(())
    }
}
