use alloy::primitives::{B256, BlockHash, BlockNumber, Sealed};
use std::convert::TryInto;
use std::path::Path;
use std::time::Duration;
use vise::Unit;
use vise::{Buckets, Histogram, Metrics};
use zksync_os_genesis::Genesis;
use zksync_os_interface::types::BlockContext;
use zksync_os_metadata::NODE_SEMVER_VERSION;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::{NamedColumnFamily, WriteBatch};
use zksync_os_storage_api::{ReadReplay, ReplayRecord, WriteReplay};
use zksync_os_types::{InteropRootsLogIndex, ProtocolSemanticVersion};

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
    ProtocolVersion,
    ForcePreimages,
    BlockOutputHash,
    StartingInteropEventIndex,
    /// Mapping from block_number to block hash.
    CanonicalHash,
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
        BlockReplayColumnFamily::ProtocolVersion,
        BlockReplayColumnFamily::BlockOutputHash,
        BlockReplayColumnFamily::ForcePreimages,
        BlockReplayColumnFamily::StartingInteropEventIndex,
        BlockReplayColumnFamily::CanonicalHash,
        BlockReplayColumnFamily::Latest,
    ];

    fn name(&self) -> &'static str {
        match self {
            BlockReplayColumnFamily::Context => "context",
            BlockReplayColumnFamily::StartingL1SerialId => "last_processed_l1_tx_id",
            BlockReplayColumnFamily::Txs => "txs",
            BlockReplayColumnFamily::NodeVersion => "node_version",
            BlockReplayColumnFamily::ProtocolVersion => "protocol_version",
            BlockReplayColumnFamily::BlockOutputHash => "block_output_hash",
            BlockReplayColumnFamily::ForcePreimages => "force_preimages",
            BlockReplayColumnFamily::StartingInteropEventIndex => "starting_interop_event_index",
            BlockReplayColumnFamily::CanonicalHash => "canonical_hash",
            BlockReplayColumnFamily::Latest => "latest",
        }
    }
}

impl BlockReplayStorage {
    /// Key under `Latest` CF for tracking the highest block number.
    const LATEST_KEY: &'static [u8] = b"latest_block";

    pub async fn new(db_path: &Path, genesis: &Genesis) -> Self {
        let db = RocksDB::<BlockReplayColumnFamily>::new(db_path)
            .expect("Failed to open BlockReplayStorage")
            .with_sync_writes();

        let this = Self { db };
        if this.latest_record_checked().is_none() {
            let genesis_tx = genesis.genesis_upgrade_tx().await;
            let genesis_context = &genesis.state().await.context;
            let genesis_hash = genesis.state().await.header.hash();
            tracing::info!(
                "block replay DB is empty, assuming start of the chain; appending genesis"
            );
            let genesis_record = ReplayRecord {
                block_context: *genesis_context,
                starting_l1_priority_id: 0,
                transactions: vec![],
                previous_block_timestamp: 0,
                node_version: NODE_SEMVER_VERSION.clone(),
                protocol_version: genesis_tx.protocol_version,
                block_output_hash: B256::ZERO,
                force_preimages: genesis_tx.force_deploy_preimages,
                starting_interop_event_index: InteropRootsLogIndex::default(),
            };
            this.write_replay_unchecked(Sealed::new_unchecked(genesis_record, genesis_hash), true);
        }
        this
    }

    fn write_replay_unchecked(&self, sealed_record: Sealed<ReplayRecord>, is_canonical: bool) {
        // Prepare record
        let (record, block_hash) = sealed_record.split();
        // TODO: We want to change the key to be block_hash for all blocks
        let db_key = if is_canonical {
            record.block_context.block_number.to_be_bytes().to_vec()
        } else {
            block_hash.0.to_vec()
        };
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

        // Batch writes: replay entry, latest pointer and canonical hash mapping
        let mut batch: WriteBatch<'_, BlockReplayColumnFamily> = self.db.new_write_batch();
        if is_canonical {
            batch.put_cf(
                BlockReplayColumnFamily::CanonicalHash,
                &record.block_context.block_number.to_be_bytes(),
                &block_hash.0,
            );
        }
        if self
            .latest_record_checked()
            .is_none_or(|l| l < record.block_context.block_number)
        {
            batch.put_cf(BlockReplayColumnFamily::Latest, Self::LATEST_KEY, &db_key);
        }
        batch.put_cf(BlockReplayColumnFamily::Context, &db_key, &context_value);
        batch.put_cf(
            BlockReplayColumnFamily::StartingL1SerialId,
            &db_key,
            &starting_l1_tx_id_value,
        );
        batch.put_cf(BlockReplayColumnFamily::Txs, &db_key, &txs_value);
        batch.put_cf(
            BlockReplayColumnFamily::NodeVersion,
            &db_key,
            &node_version_value,
        );
        batch.put_cf(
            BlockReplayColumnFamily::BlockOutputHash,
            &db_key,
            &record.block_output_hash.0,
        );
        batch.put_cf(
            BlockReplayColumnFamily::ProtocolVersion,
            &db_key,
            record.protocol_version.to_string().as_bytes(),
        );
        let force_preimages_value = bincode::encode_to_vec(
            &StorageForcePreimages {
                preimages: record.force_preimages,
            },
            bincode::config::standard(),
        )
        .expect("Failed to serialize record.force_preimages");
        batch.put_cf(
            BlockReplayColumnFamily::ForcePreimages,
            &db_key,
            &force_preimages_value,
        );

        let starting_interop_event_index_value = bincode::serde::encode_to_vec(
            &record.starting_interop_event_index,
            bincode::config::standard(),
        )
        .expect("Failed to serialize record.starting_interop_event_index");
        batch.put_cf(
            BlockReplayColumnFamily::StartingInteropEventIndex,
            &db_key,
            &starting_interop_event_index_value,
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

    /// Given `block_number` retrieve block's hash.
    fn get_canonical_block_hash(&self, block_number: BlockNumber) -> BlockHash {
        let get_hash = |block_number: BlockNumber| -> Option<BlockHash> {
            let key = block_number.to_be_bytes();
            self.db
                .get_cf(BlockReplayColumnFamily::CanonicalHash, &key)
                .expect("Failed to read from CanonicalHash DB")
                .map(|bytes| BlockHash::from_slice(&bytes))
        };

        get_hash(block_number).unwrap_or_else(|| {
            //There are some rare corner cases related to rebuilds right after introducing the CF
            //I choose to panic in such cases as I really don't expect them to happen
            let latest = self.latest_record();
            assert!(latest > block_number);
            let _ = get_hash(latest).expect("Cannot guarantee correctness until latest is updated");
            BlockHash::from(
                *self
                    .get_context(block_number + 1)
                    .expect("Record is missing")
                    .block_hashes
                    .0
                    .last()
                    .unwrap(),
            )
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

    fn get_replay_record_by_key(
        &self,
        block_number: u64,
        db_key: Option<Vec<u8>>,
    ) -> Option<ReplayRecord> {
        let key = db_key.unwrap_or_else(|| block_number.to_be_bytes().to_vec());
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

        let protocol_version = if let Some(version) = self
            .db
            .get_cf(BlockReplayColumnFamily::ProtocolVersion, &key)
            .expect("Failed to read from ProtocolVersion CF")
        {
            String::from_utf8(version)
                .expect("Failed to deserialize protocol version")
                .parse()
                .expect("Failed to parse protocol version")
        } else {
            // TODO: temporary sanity check. This code is written when this CF is just introduced, so
            // on some live nodes storage may not have this CF populated for historical blocks.
            // Check if protocol version if available for genesis block -> it if is, then missing key
            // is a bug and we should panic; if not, we can assume all historical blocks are missing it and
            // default to latest version.
            let genesis_block = 0u64.to_be_bytes();
            let genesis_protocol_version = self
                .db
                .get_cf(BlockReplayColumnFamily::ProtocolVersion, &genesis_block)
                .expect("Failed to read from ProtocolVersion CF for genesis block");
            if genesis_protocol_version.is_some() {
                panic!(
                    "ProtocolVersion missing for block {block_number} despite being present for genesis block"
                );
            }

            ProtocolSemanticVersion::legacy_genesis_version()
        };

        let force_preimages = if let Some(preimages) = self
            .db
            .get_cf(BlockReplayColumnFamily::ForcePreimages, &key)
            .expect("Failed to read from ForcePreimages CF")
        {
            let stored: StorageForcePreimages =
                bincode::decode_from_slice(&preimages, bincode::config::standard())
                    .expect("Failed to deserialize force preimages")
                    .0;
            stored.preimages
        } else {
            // We assume that protocol check would panic if DB is inconsistent state.
            vec![]
        };

        let block_output_hash = self
            .db
            .get_cf(BlockReplayColumnFamily::BlockOutputHash, &key)
            .expect("Failed to read from BlockOutputHash CF")
            .expect("BlockOutputHash must be written atomically with Context");

        let starting_interop_event_index = if let Some(starting_interop_event_index) = self
            .db
            .get_cf(BlockReplayColumnFamily::StartingInteropEventIndex, &key)
            .expect("Failed to read from StartingInteropEventIndex CF")
        {
            let stored: InteropRootsLogIndex = bincode::serde::decode_from_slice(
                &starting_interop_event_index,
                bincode::config::standard(),
            )
            .expect("Failed to deserialize starting interop event index")
            .0;
            stored
        } else {
            InteropRootsLogIndex::default()
        };

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
            protocol_version,
            block_output_hash: B256::from_slice(&block_output_hash),
            force_preimages,
            starting_interop_event_index,
        })
    }

    fn latest_record(&self) -> BlockNumber {
        // This is guaranteed to be non-`None` because genesis is always inserted on storage initialization.
        self.latest_record_checked()
            .expect("no blocks in BlockReplayStorage")
    }
}

impl WriteReplay for BlockReplayStorage {
    fn write(&self, sealed_record: Sealed<ReplayRecord>, override_allowed: bool) -> bool {
        let latency_observer = BLOCK_REPLAY_ROCKS_DB_METRICS.get_latency.start();
        let block_record = sealed_record.as_ref();
        let block_context = &sealed_record.block_context;
        let current_latest_record = self.latest_record();
        if block_context.block_number <= current_latest_record && !override_allowed {
            // todo: consider asserting that the passed `ReplayRecord` matches the one currently stored
            tracing::debug!(
                block_number = block_context.block_number,
                "not appending block: already exists in block replay storage",
            );
            return false;
        } else if block_context.block_number > current_latest_record + 1 {
            panic!(
                "tried to append non-sequential replay record: {} > {}",
                block_context.block_number,
                current_latest_record + 1
            );
        }

        if block_context.block_number <= current_latest_record {
            let old_record = self
                .get_replay_record(block_context.block_number)
                .expect("Old record must exist");
            if &old_record != block_record {
                let old_record_hash = self.get_canonical_block_hash(block_context.block_number);
                let old_record_hash_hex = alloy::hex::encode_prefixed(old_record_hash.0);
                tracing::warn!(
                    block_number = block_context.block_number,
                    old_record_hash_hex,
                    "Overriding existing block replay record",
                );
                self.write_replay_unchecked(
                    Sealed::new_unchecked(old_record, old_record_hash),
                    false,
                );
            }
        }

        self.write_replay_unchecked(sealed_record, true);
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

#[derive(Debug, bincode::Encode, bincode::Decode)]
pub struct StorageForcePreimages {
    #[bincode(with_serde)]
    pub preimages: Vec<(B256, Vec<u8>)>,
}
