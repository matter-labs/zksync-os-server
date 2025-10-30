use alloy::primitives::{B256, BlockNumber};
use std::convert::TryInto;
use std::path::Path;
use std::time::Duration;
use vise::Unit;
use vise::{Buckets, Histogram, Metrics};
use zksync_os_genesis::Genesis;
use zksync_os_interface::types::BlockContext;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::{NamedColumnFamily, WriteBatch};
use zksync_os_storage_api::{ReadReplay, ReplayRecord, WriteReplay};

/// A write-ahead log storing [`ReplayRecord`]s.
///
/// Used for (but not limited to) the following purposes:
/// * Sequencer's state recovery (provides all information needed to replay a block after restart).
/// * Execution environment for historical blocks (e.g., as required in `eth_call`).
/// * Provides replay records for MainNode -> EN synchronization.
///
/// Implements [`ReadReplay`] and [`WriteReplay`] traits and satisfies their requirements for the
/// entire lifetime of the disk containing RocksDB data underpinning this storage (see
/// [`ReadReplay`]'s documentation for details on lifetime). Assumes no external manipulation with
/// on-disk data.
///
/// Writes are synchronous to accommodate the lifetime requirement above. Otherwise, an OS crash
/// can cause data to be lost (not being written on disk), thus rolling back an already appended replay
/// record. See [RocksDB docs](https://github.com/facebook/rocksdb/wiki/basic-operations#synchronous-writes)
/// for more info.
#[derive(Clone, Debug)]
pub struct BlockReplayStorage {
    db: RocksDB<BlockReplayColumnFamily>,
}

/// Column families for storage of block replay commands.
#[derive(Copy, Clone, Debug)]
pub enum BlockReplayColumnFamily {
    Context,
    StartingL1SerialId,
    Txs,
    NodeVersion,
    BlockOutputHash,
    /// Stores the latest appended block number under a fixed key.
    Latest,
}

impl NamedColumnFamily for BlockReplayColumnFamily {
    const DB_NAME: &'static str = "block_replay_wal";
    const ALL: &'static [Self] = &[
        BlockReplayColumnFamily::Context,
        BlockReplayColumnFamily::StartingL1SerialId,
        BlockReplayColumnFamily::Txs,
        BlockReplayColumnFamily::NodeVersion,
        BlockReplayColumnFamily::BlockOutputHash,
        BlockReplayColumnFamily::Latest,
    ];

    fn name(&self) -> &'static str {
        match self {
            BlockReplayColumnFamily::Context => "context",
            BlockReplayColumnFamily::StartingL1SerialId => "last_processed_l1_tx_id",
            BlockReplayColumnFamily::Txs => "txs",
            BlockReplayColumnFamily::NodeVersion => "node_version",
            BlockReplayColumnFamily::BlockOutputHash => "block_output_hash",
            BlockReplayColumnFamily::Latest => "latest",
        }
    }
}

impl BlockReplayStorage {
    /// Key under `Latest` CF for tracking the highest block number.
    const LATEST_KEY: &'static [u8] = b"latest_block";

    pub async fn new(db_path: &Path, genesis: &Genesis, node_version: semver::Version) -> Self {
        let db = RocksDB::<BlockReplayColumnFamily>::new(db_path)
            .expect("Failed to open BlockReplayStorage")
            .with_sync_writes();

        let this = Self { db };
        if this.latest_record_checked().is_none() {
            let genesis_context = &genesis.state().await.context;
            tracing::info!(
                "block replay DB is empty, assuming start of the chain; appending genesis"
            );
            this.write_replay_unchecked(ReplayRecord {
                block_context: *genesis_context,
                starting_l1_priority_id: 0,
                transactions: vec![],
                previous_block_timestamp: 0,
                node_version,
                block_output_hash: B256::ZERO,
            })
        }
        this
    }

    fn write_replay_unchecked(&self, record: ReplayRecord) {
        // Prepare record
        let block_num = record.block_context.block_number.to_be_bytes();
        let context_value =
            bincode::serde::encode_to_vec(record.block_context, bincode::config::standard())
                .expect("Failed to serialize record.context");
        let starting_l1_tx_id_value = bincode::serde::encode_to_vec(
            record.starting_l1_priority_id,
            bincode::config::standard(),
        )
        .expect("Failed to serialize record.last_processed_l1_tx_id");
        let txs_value = bincode::encode_to_vec(&record.transactions, bincode::config::standard())
            .expect("Failed to serialize record.transactions");
        let node_version_value = record.node_version.to_string().as_bytes().to_vec();

        // Batch both writes: replay entry and latest pointer
        let mut batch: WriteBatch<'_, BlockReplayColumnFamily> = self.db.new_write_batch();
        if self
            .latest_record_checked()
            .is_none_or(|l| l < record.block_context.block_number)
        {
            batch.put_cf(
                BlockReplayColumnFamily::Latest,
                Self::LATEST_KEY,
                &block_num,
            );
        }
        batch.put_cf(BlockReplayColumnFamily::Context, &block_num, &context_value);
        batch.put_cf(
            BlockReplayColumnFamily::StartingL1SerialId,
            &block_num,
            &starting_l1_tx_id_value,
        );
        batch.put_cf(BlockReplayColumnFamily::Txs, &block_num, &txs_value);
        batch.put_cf(
            BlockReplayColumnFamily::NodeVersion,
            &block_num,
            &node_version_value,
        );
        batch.put_cf(
            BlockReplayColumnFamily::BlockOutputHash,
            &block_num,
            &record.block_output_hash.0,
        );

        self.db
            .write(batch)
            .expect("Failed to write to block replay storage");
    }

    /// Returns the greatest block number that has been appended, or `None` if empty.
    /// This can only return `None` on the very first start before genesis got inserted.
    fn latest_record_checked(&self) -> Option<BlockNumber> {
        self.db
            .get_cf(BlockReplayColumnFamily::Latest, Self::LATEST_KEY)
            .expect("Cannot read from DB")
            .map(|bytes| {
                assert_eq!(bytes.len(), 8);
                let arr: [u8; 8] = bytes.as_slice().try_into().unwrap();
                u64::from_be_bytes(arr)
            })
    }
}

impl ReadReplay for BlockReplayStorage {
    fn get_context(&self, block_number: BlockNumber) -> Option<BlockContext> {
        let key = block_number.to_be_bytes();
        self.db
            .get_cf(BlockReplayColumnFamily::Context, &key)
            .expect("Cannot read from DB")
            .map(|bytes| {
                bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
                    .expect("Failed to deserialize context")
            })
            .map(|(context, _)| context)
    }

    fn get_replay_record(&self, block_number: u64) -> Option<ReplayRecord> {
        let key = block_number.to_be_bytes();
        let Some(block_context) = self
            .db
            .get_cf(BlockReplayColumnFamily::Context, &key)
            .expect("Failed to read from Context CF")
        else {
            // Writes are atomic, so if we can't read the context, we can't read the rest of the
            // replay record anyway.
            return None;
        };

        // Writes are atomic and, since block context was read successfully, the rest of the replay
        // record should be present too. Hence, we can safely unwrap here.
        let starting_l1_priority_id = self
            .db
            .get_cf(BlockReplayColumnFamily::StartingL1SerialId, &key)
            .expect("Failed to read from LastProcessedL1TxId CF")
            .expect("StartingL1SerialId must be written atomically with Context");
        let transactions = self
            .db
            .get_cf(BlockReplayColumnFamily::Txs, &key)
            .expect("Failed to read from Txs CF")
            .expect("Txs must be written atomically with Context");
        // todo: save `previous_block_timestamp` as another column in the next breaking change to
        //       replay record format
        let previous_block_timestamp = if block_number == 0 {
            // Genesis does not have previous block and this value should never be used, but we
            // return `0` here for the flow to work.
            0
        } else {
            self.get_context(block_number - 1)
                .map(|context| context.timestamp)
                .unwrap_or(0)
        };

        let node_version = self
            .db
            .get_cf(BlockReplayColumnFamily::NodeVersion, &key)
            .expect("Failed to read from NodeVersion CF")
            .expect("NodeVersion must be written atomically with Context");
        let block_output_hash = self
            .db
            .get_cf(BlockReplayColumnFamily::BlockOutputHash, &key)
            .expect("Failed to read from BlockOutputHash CF")
            .expect("BlockOutputHash must be written atomically with Context");

        Some(ReplayRecord {
            block_context: bincode::serde::decode_from_slice(
                &block_context,
                bincode::config::standard(),
            )
            .expect("Failed to deserialize context")
            .0,
            starting_l1_priority_id: bincode::serde::decode_from_slice(
                &starting_l1_priority_id,
                bincode::config::standard(),
            )
            .expect("Failed to deserialize context")
            .0,
            transactions: bincode::decode_from_slice(&transactions, bincode::config::standard())
                .expect("Failed to deserialize transactions")
                .0,
            previous_block_timestamp,
            node_version: String::from_utf8(node_version)
                .expect("Failed to deserialize node version")
                .parse()
                .expect("Failed to parse node version"),
            block_output_hash: B256::from_slice(&block_output_hash),
        })
    }

    fn latest_record(&self) -> BlockNumber {
        // This is guaranteed to be non-`None` because genesis is always inserted on storage initialization.
        self.latest_record_checked()
            .expect("no blocks in BlockReplayStorage")
    }
}

impl WriteReplay for BlockReplayStorage {
    fn write(&self, record: ReplayRecord, override_allowed: bool) -> bool {
        let latency_observer = BLOCK_REPLAY_ROCKS_DB_METRICS.get_latency.start();
        let current_latest_record = self.latest_record();
        if record.block_context.block_number <= current_latest_record && !override_allowed {
            // todo: consider asserting that the passed `ReplayRecord` matches the one currently stored
            tracing::debug!(
                block_number = record.block_context.block_number,
                "not appending block: already exists in block replay storage",
            );
            return false;
        } else if record.block_context.block_number > current_latest_record + 1 {
            panic!(
                "tried to append non-sequential replay record: {} > {}",
                record.block_context.block_number,
                current_latest_record + 1
            );
        }

        if record.block_context.block_number <= current_latest_record {
            tracing::info!(
                "Overriding existing block replay record {}",
                record.block_context.block_number
            );
        }

        self.write_replay_unchecked(record);
        latency_observer.observe();
        true
    }
}

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);

#[derive(Debug, Metrics)]
#[metrics(prefix = "block_replay_storage")]
pub struct BlockReplayRocksDBMetrics {
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub get_latency: Histogram<Duration>,

    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub set_latency: Histogram<Duration>,
}

#[vise::register]
pub static BLOCK_REPLAY_ROCKS_DB_METRICS: vise::Global<BlockReplayRocksDBMetrics> =
    vise::Global::new();
