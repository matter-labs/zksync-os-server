use crate::execution::metrics::BLOCK_REPLAY_ROCKS_DB_METRICS;
use crate::model::{BlockCommand, ReplayRecord};
use alloy::eips::{Decodable2718, Encodable2718};
use futures::stream::{self, BoxStream, StreamExt};
use std::convert::TryInto;
use zk_os_forward_system::run::BatchContext;
use zksync_os_types::ZkEnvelope;
use zksync_storage::RocksDB;
use zksync_storage::db::{NamedColumnFamily, WriteBatch};

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
    /// Stores the latest appended block number under a fixed key.
    Latest,
}

impl NamedColumnFamily for BlockReplayColumnFamily {
    const DB_NAME: &'static str = "block_replay_wal";
    const ALL: &'static [Self] = &[
        BlockReplayColumnFamily::Context,
        BlockReplayColumnFamily::StartingL1SerialId,
        BlockReplayColumnFamily::Txs,
        BlockReplayColumnFamily::Latest,
    ];

    fn name(&self) -> &'static str {
        match self {
            BlockReplayColumnFamily::Context => "context",
            BlockReplayColumnFamily::StartingL1SerialId => "last_processed_l1_tx_id",
            BlockReplayColumnFamily::Txs => "txs",
            BlockReplayColumnFamily::Latest => "latest",
        }
    }
}

impl BlockReplayStorage {
    /// Key under `Latest` CF for tracking the highest block number.
    const LATEST_KEY: &'static [u8] = b"latest_block";

    pub fn new(db: RocksDB<BlockReplayColumnFamily>) -> Self {
        Self { db }
    }
    /// Appends a replay command (context + raw transactions) to the WAL.
    /// Also updates the Latest CF. Returns the corresponding ReplayRecord.
    pub fn append_replay(&self, record: ReplayRecord) {
        let latency = BLOCK_REPLAY_ROCKS_DB_METRICS.get_latency.start();
        assert!(!record.transactions.is_empty());

        let current_latest_block = self.latest_block().unwrap_or(0);

        if record.block_context.block_number <= current_latest_block {
            tracing::debug!(
                "Not appending block {}: already exists in WAL",
                record.block_context.block_number
            );
            return;
        }

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
        let txs_2718_encoded = record
            .transactions
            .into_iter()
            .map(|tx| tx.inner.encoded_2718())
            .collect::<Vec<_>>();
        let txs_value =
            bincode::serde::encode_to_vec(&txs_2718_encoded, bincode::config::standard())
                .expect("Failed to serialize record.transactions");

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

        self.db.write(batch).expect("Failed to write to WAL");
        latency.observe();
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

    pub fn get_context(&self, block_number: u64) -> Option<BatchContext> {
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

    pub fn get_replay_record(&self, block_number: u64) -> Option<ReplayRecord> {
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

        match (context_result, last_processed_l1_tx_result, txs_result) {
            (Some(bytes_context), Some(bytes_starting_l1_tx), Some(bytes_txs)) => {
                Some(ReplayRecord {
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
                    transactions: bincode::serde::decode_from_slice::<Vec<Vec<u8>>, _>(
                        &bytes_txs,
                        bincode::config::standard(),
                    )
                    .expect("Failed to deserialize transactions")
                    .0
                    .into_iter()
                    .map(|bytes| {
                        ZkEnvelope::decode_2718(&mut bytes.as_slice())
                            .expect("Failed to decode 2718 transaction")
                            .try_into_recovered()
                            .expect("Failed to recover transaction's signer")
                    })
                    .collect(),
                })
            }
            (None, None, None) => None,
            _ => panic!("Inconsistent state: Context and Txs must be written atomically"),
        }
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
}
