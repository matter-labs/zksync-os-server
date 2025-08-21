use alloy::primitives::{B256, BlockNumber};
use futures::Stream;
use futures::stream::{self, BoxStream, StreamExt};
use pin_project::pin_project;
use ruint::aliases::U256;
use std::convert::TryInto;
use std::task::Poll;
use std::time::Duration;
use tokio::pin;
use tokio::time::{Instant, Sleep};
use zk_ee::system::metadata::BlockMetadataFromOracle;
use zk_os_forward_system::run::BlockContext;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::{NamedColumnFamily, WriteBatch};
use zksync_os_sequencer::execution::metrics::BLOCK_REPLAY_ROCKS_DB_METRICS;
use zksync_os_sequencer::model::blocks::BlockCommand;
use zksync_os_storage_api::{ReadReplay, ReplayRecord, WriteReplay};

/// A write-ahead log storing BlockReplayData.
/// It is then used for:
///  * Context + Transaction list: sequencer recovery.
///  * Context: provides execution environment for `eth_call`s against older blocks
///
/// Acts as canonization provider for centralized sequencers.
/// Writes must be synchronous.
///
#[derive(Clone, Debug)]
pub struct BlockReplayStorage {
    db: RocksDB<BlockReplayColumnFamily>,
}

/// Column families for WAL storage of block replay commands.
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

    pub fn new(
        db: RocksDB<BlockReplayColumnFamily>,
        chain_id: u64,
        node_version: semver::Version,
    ) -> Self {
        let this = Self { db };
        if this.latest_block().is_none() {
            tracing::info!(
                "block replay DB is empty, assuming start of the chain; appending genesis"
            );
            this.append_replay_unchecked(ReplayRecord {
                // todo: save real genesis here once we have genesis logic
                block_context: BlockMetadataFromOracle {
                    chain_id,
                    block_number: 0,
                    block_hashes: Default::default(),
                    timestamp: 0,
                    eip1559_basefee: U256::from(0),
                    gas_per_pubdata: U256::from(0),
                    native_price: U256::from(1),
                    coinbase: Default::default(),
                    gas_limit: 100_000_000,
                    pubdata_limit: 100_000_000,
                    mix_hash: Default::default(),
                },
                starting_l1_priority_id: 0,
                transactions: vec![],
                previous_block_timestamp: 0,
                node_version,
                block_output_hash: B256::ZERO,
            })
        }
        this
    }

    fn append_replay_unchecked(&self, record: ReplayRecord) {
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
        batch.put_cf(
            BlockReplayColumnFamily::Latest,
            Self::LATEST_KEY,
            &block_num,
        );
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

        self.db.write(batch).expect("Failed to write to WAL");
    }

    /// Returns the greatest block number that has been appended, or None if empty.
    pub fn latest_block(&self) -> Option<u64> {
        self.db
            .get_cf(BlockReplayColumnFamily::Latest, Self::LATEST_KEY)
            .expect("Cannot read from DB")
            .map(|bytes| {
                assert_eq!(bytes.len(), 8);
                let arr: [u8; 8] = bytes.as_slice().try_into().unwrap();
                u64::from_be_bytes(arr)
            })
    }

    /// Streams all replay commands with block_number ≥ `start`, in ascending block order - used for state recovery
    pub fn replay_commands_from(&self, start: u64) -> BoxStream<BlockCommand> {
        let latest = self.latest_block().unwrap_or(0);
        let stream = stream::iter(start..=latest).filter_map(move |block_num| {
            let record = self.get_replay_record(block_num);
            match record {
                Some(record) => {
                    futures::future::ready(Some(BlockCommand::Replay(Box::new(record))))
                }
                None => futures::future::ready(None),
            }
        });
        Box::pin(stream)
    }

    /// Streams all replay commands with block_number ≥ `start`, in ascending block order and waits for new ones on running out
    pub fn replay_commands_forever(&self, start: BlockNumber) -> BoxStream<ReplayRecord> {
        #[pin_project]
        struct BlockStream {
            replays: BlockReplayStorage,
            current_block: BlockNumber,
            #[pin]
            sleep: Sleep,
        }
        impl Stream for BlockStream {
            type Item = ReplayRecord;

            fn poll_next(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                let mut this = self.project();
                if let Some(record) = this.replays.get_replay_record(*this.current_block) {
                    *this.current_block += 1;
                    Poll::Ready(Some(record))
                } else {
                    // TODO: would be nice to be woken up only when the next block is available
                    this.sleep
                        .as_mut()
                        .reset(Instant::now() + Duration::from_millis(50));
                    assert_eq!(this.sleep.poll(cx), Poll::Pending);
                    Poll::Pending
                }
            }
        }

        Box::pin(BlockStream {
            replays: self.clone(),
            current_block: start,
            sleep: tokio::time::sleep(Duration::from_millis(50)),
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
        let context_result = self
            .db
            .get_cf(BlockReplayColumnFamily::Context, &key)
            .expect("Failed to read from Context CF");
        let last_processed_l1_tx_result = self
            .db
            .get_cf(BlockReplayColumnFamily::StartingL1SerialId, &key)
            .expect("Failed to read from LastProcessedL1TxId CF");
        let txs_result = self
            .db
            .get_cf(BlockReplayColumnFamily::Txs, &key)
            .expect("Failed to read from Txs CF");
        let previous_block_timestamp = self
            .get_context(block_number)
            .map(|context| context.timestamp)
            .unwrap_or(0);

        let node_version_result = self
            .db
            .get_cf(BlockReplayColumnFamily::NodeVersion, &key)
            .expect("Failed to read from NodeVersion CF");
        let block_output_hash_result = self
            .db
            .get_cf(BlockReplayColumnFamily::BlockOutputHash, &key)
            .expect("Failed to read from BlockOutputHash CF");

        match (
            context_result,
            last_processed_l1_tx_result,
            txs_result,
            block_output_hash_result,
        ) {
            (
                Some(bytes_context),
                Some(bytes_starting_l1_tx),
                Some(bytes_txs),
                Some(bytes_block_output_hash),
            ) => Some(ReplayRecord {
                block_context: bincode::serde::decode_from_slice(
                    &bytes_context,
                    bincode::config::standard(),
                )
                .expect("Failed to deserialize context")
                .0,
                starting_l1_priority_id: bincode::serde::decode_from_slice(
                    &bytes_starting_l1_tx,
                    bincode::config::standard(),
                )
                .expect("Failed to deserialize context")
                .0,
                transactions: bincode::decode_from_slice(&bytes_txs, bincode::config::standard())
                    .expect("Failed to deserialize transactions")
                    .0,
                previous_block_timestamp,
                node_version: node_version_result
                    .map(|bytes| {
                        String::from_utf8(bytes)
                            .expect("Failed to deserialize node version")
                            .parse()
                            .expect("Failed to parse node version")
                    })
                    .unwrap_or_else(|| semver::Version::new(0, 1, 0)),
                block_output_hash: B256::from_slice(&bytes_block_output_hash),
            }),
            (None, None, None, None) => None,
            _ => panic!("Inconsistent state: Context and Txs must be written atomically"),
        }
    }
}

impl WriteReplay for BlockReplayStorage {
    fn append(&self, record: ReplayRecord) {
        let latency_observer = BLOCK_REPLAY_ROCKS_DB_METRICS.get_latency.start();
        assert!(!record.transactions.is_empty());

        let current_latest_block = self.latest_block().unwrap_or(0);

        if record.block_context.block_number <= current_latest_block {
            tracing::debug!(
                "Not appending block {}: already exists in WAL",
                record.block_context.block_number
            );
            return;
        }
        self.append_replay_unchecked(record);
        latency_observer.observe();
    }
}
