use alloy::primitives::{Address, B256, BlockHash, BlockNumber, Sealed, U256};
use std::convert::TryInto;
use std::path::Path;
use std::time::Duration;
use vise::Unit;
use vise::{Buckets, Histogram, Metrics};
use zksync_os_genesis::Genesis;
use zksync_os_metadata::NODE_SEMVER_VERSION;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::{NamedColumnFamily, WriteBatch};
use zksync_os_storage_api::{BlockContext, BlockHashes, ReadReplay, ReplayRecord, WriteReplay};
use zksync_os_types::{BlockStartCursors, ProtocolSemanticVersion};

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
    /// Shared by all blocks; stripped rows don't persist it (see [`StoredBlockContextV2`]).
    chain_id: u64,
}

/// Column families for storage of block replay commands.
///
/// TODO(RocksDB migration): The four `Starting*` column families below correspond to fields
/// in [`BlockStartCursors`]. They are stored separately for historical reasons (each was added
/// independently). A future migration should consolidate them into a single column family
/// serializing the entire `BlockStartCursors` struct.
#[derive(Copy, Clone, Debug)]
pub enum BlockReplayColumnFamily {
    /// Full [`BlockContext`], including the 256 previous block hashes (~8 KiB per block).
    /// For canonical rows this is a rollback-safety copy — reads prefer [`Self::ContextV2`].
    /// Non-canonical (hash-keyed) rows live only here.
    ///
    /// TODO(RocksDB migration): stop writing canonical rows here and delete existing ones.
    Context,
    /// Stripped [`BlockContext`] for canonical rows: everything except the 256 previous block
    /// hashes, which are derivable data and get reconstructed from [`Self::CanonicalHash`] on
    /// read (see [`StoredBlockContextV2`]).
    ContextV2,
    StartingL1SerialId,
    Txs,
    NodeVersion,
    ProtocolVersion,
    ForcePreimages,
    BlockOutputHash,
    StartingInteropRootId,
    StartingMigrationNumber,
    StartingInteropFeeNumber,
    /// Mapping from block_number to block hash.
    CanonicalHash,
    /// Stores the latest appended block number under a fixed key.
    Latest,
}

impl NamedColumnFamily for BlockReplayColumnFamily {
    const DB_NAME: &'static str = "block_replay_wal";
    const ALL: &'static [Self] = &[
        BlockReplayColumnFamily::Context,
        BlockReplayColumnFamily::ContextV2,
        BlockReplayColumnFamily::StartingL1SerialId,
        BlockReplayColumnFamily::Txs,
        BlockReplayColumnFamily::NodeVersion,
        BlockReplayColumnFamily::ProtocolVersion,
        BlockReplayColumnFamily::BlockOutputHash,
        BlockReplayColumnFamily::ForcePreimages,
        BlockReplayColumnFamily::StartingInteropRootId,
        BlockReplayColumnFamily::StartingMigrationNumber,
        BlockReplayColumnFamily::StartingInteropFeeNumber,
        BlockReplayColumnFamily::CanonicalHash,
        BlockReplayColumnFamily::Latest,
    ];

    fn name(&self) -> &'static str {
        match self {
            BlockReplayColumnFamily::Context => "context",
            BlockReplayColumnFamily::ContextV2 => "context_v2",
            BlockReplayColumnFamily::StartingL1SerialId => "last_processed_l1_tx_id",
            BlockReplayColumnFamily::Txs => "txs",
            BlockReplayColumnFamily::NodeVersion => "node_version",
            BlockReplayColumnFamily::ProtocolVersion => "protocol_version",
            BlockReplayColumnFamily::BlockOutputHash => "block_output_hash",
            BlockReplayColumnFamily::ForcePreimages => "force_preimages",
            BlockReplayColumnFamily::StartingInteropRootId => "starting_interop_root_id",
            BlockReplayColumnFamily::StartingMigrationNumber => "starting_migration_number",
            BlockReplayColumnFamily::StartingInteropFeeNumber => "starting_interop_fee_number",
            BlockReplayColumnFamily::CanonicalHash => "canonical_hash",
            BlockReplayColumnFamily::Latest => "latest",
        }
    }
}

impl BlockReplayStorage {
    /// Key under `Latest` CF for tracking the highest block number.
    const LATEST_KEY: &'static [u8] = b"latest_block";

    pub async fn new(db_path: &Path, genesis: &Genesis) -> (Self, Option<Sealed<ReplayRecord>>) {
        let db = RocksDB::<BlockReplayColumnFamily>::new(db_path)
            .expect("Failed to open BlockReplayStorage")
            .with_sync_writes();

        let this = Self {
            db,
            chain_id: genesis.chain_id(),
        };
        let inserted_genesis = if this.latest_record_checked().is_none() {
            let genesis_tx = genesis.genesis_upgrade_tx().await;
            let genesis_context = &genesis.state().await.context;
            let genesis_hash = genesis.state().await.header.hash();
            tracing::info!(
                "block replay DB is empty, assuming start of the chain; appending genesis"
            );
            let genesis_record = ReplayRecord {
                block_context: *genesis_context,
                transactions: vec![],
                previous_block_timestamp: 0,
                node_version: NODE_SEMVER_VERSION.clone(),
                protocol_version: genesis_tx.protocol_version.clone(),
                block_output_hash: B256::ZERO,
                force_preimages: genesis_tx.force_deploy_preimages.clone(),
                starting_cursors: BlockStartCursors::default(),
            };
            let sealed_genesis_record = Sealed::new_unchecked(genesis_record, genesis_hash);
            this.write_replay_unchecked(sealed_genesis_record.clone(), true);
            Some(sealed_genesis_record)
        } else {
            None
        };
        (this, inserted_genesis)
    }

    /// Opens replay storage without inserting genesis.
    ///
    /// This is intended for recovery tooling that rebuilds the DB from archived replay records and
    /// writes the recovered chain from genesis upward.
    pub fn new_without_genesis(db_path: &Path, chain_id: u64) -> Self {
        let db = RocksDB::<BlockReplayColumnFamily>::new(db_path)
            .expect("Failed to open BlockReplayStorage")
            .with_sync_writes();
        Self { db, chain_id }
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
        // TODO(RocksDB migration): BlockStartCursors fields are stored in separate column families
        // for historical reasons. A future migration should consolidate them into a single CF
        // serializing the entire BlockStartCursors struct.
        let starting_l1_tx_id_value = bincode::serde::encode_to_vec(
            record.starting_cursors.l1_priority_id,
            bincode::config::standard(),
        )
        .expect("Failed to serialize record.starting_cursors.l1_priority_id");
        let txs_value = bincode::encode_to_vec(&record.transactions, bincode::config::standard())
            .expect("Failed to serialize record.transactions");
        let node_version_value = record.node_version.to_string().as_bytes().to_vec();

        // Batch writes: replay entry, latest pointer and canonical hash mapping
        let mut batch: WriteBatch<'_, BlockReplayColumnFamily> = self.db.new_write_batch();
        if is_canonical {
            // Expand phase of dropping the persisted block hashes: canonical rows are
            // dual-written. Reads prefer this stripped row; the full legacy row (below) keeps
            // rollback to older binaries safe until a migration removes it.
            let stripped_context_value = bincode::encode_to_vec(
                StoredBlockContextV2::strip(record.block_context),
                bincode::config::standard(),
            )
            .expect("Failed to serialize stripped record.context");
            batch.put_cf(
                BlockReplayColumnFamily::ContextV2,
                &db_key,
                &stripped_context_value,
            );
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

        let starting_interop_root_id_value = bincode::serde::encode_to_vec(
            record.starting_cursors.interop_root_id,
            bincode::config::standard(),
        )
        .expect("Failed to serialize record.starting_cursors.interop_root_id");
        batch.put_cf(
            BlockReplayColumnFamily::StartingInteropRootId,
            &db_key,
            &starting_interop_root_id_value,
        );

        let starting_migration_number_value = bincode::serde::encode_to_vec(
            record.starting_cursors.migration_number,
            bincode::config::standard(),
        )
        .expect("Failed to serialize record.starting_cursors.migration_number");
        batch.put_cf(
            BlockReplayColumnFamily::StartingMigrationNumber,
            &db_key,
            &starting_migration_number_value,
        );

        let starting_interop_fee_number_value = bincode::serde::encode_to_vec(
            record.starting_cursors.interop_fee_number,
            bincode::config::standard(),
        )
        .expect("Failed to serialize record.starting_cursors.interop_fee_number");
        batch.put_cf(
            BlockReplayColumnFamily::StartingInteropFeeNumber,
            &db_key,
            &starting_interop_fee_number_value,
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
        self.get_canonical_block_hash_opt(block_number)
            .unwrap_or_else(|| {
                //There are some rare corner cases related to rebuilds right after introducing the CF
                //I choose to panic in such cases as I really don't expect them to happen
                let latest = self.latest_record();
                assert!(latest > block_number);
                let _ = self
                    .get_canonical_block_hash_opt(latest)
                    .expect("Cannot guarantee correctness until latest is updated");
                // A missing entry implies the block was appended before `CanonicalHash` was
                // introduced, so its successor's row embeds the hash as the last element.
                BlockHash::from(
                    *self
                        .get_legacy_context(&(block_number + 1).to_be_bytes())
                        .expect("Record is missing")
                        .block_hashes
                        .0
                        .last()
                        .unwrap(),
                )
            })
    }

    fn get_canonical_block_hash_opt(&self, block_number: BlockNumber) -> Option<BlockHash> {
        self.db
            .get_cf(
                BlockReplayColumnFamily::CanonicalHash,
                &block_number.to_be_bytes(),
            )
            .expect("Failed to read from CanonicalHash DB")
            .map(|bytes| BlockHash::from_slice(&bytes))
    }

    /// Reconstructs the 256 previous block hashes for the block stored under `key` from the
    /// `CanonicalHash` CF. The hash of block `M` lands at index `M + 256 - block_number` (most
    /// recent last); slots before genesis stay zero, mirroring how the genesis context is
    /// initialized.
    ///
    /// Entries can only be missing for blocks appended before `CanonicalHash` was introduced;
    /// those slots are taken from the hashes embedded in the row's legacy copy.
    fn read_block_hashes(&self, block_number: BlockNumber, key: &[u8]) -> BlockHashes {
        let mut hashes = [U256::ZERO; 256];
        let first = block_number.saturating_sub(256);
        let keys = (first..block_number)
            .map(|number| number.to_be_bytes())
            .collect::<Vec<_>>();
        let results = self
            .db
            .multi_get_cf(BlockReplayColumnFamily::CanonicalHash, keys.iter());
        let mut embedded = None;
        for (number, result) in (first..block_number).zip(results) {
            let index = (number + 256 - block_number) as usize;
            hashes[index] = match result.expect("Failed to read from CanonicalHash DB") {
                Some(bytes) => U256::from_be_slice(&bytes),
                None => {
                    let embedded = embedded.get_or_insert_with(|| {
                        // Only expected for blocks appended before `CanonicalHash` was
                        // introduced, i.e. only on chains that haven't produced a block since.
                        tracing::warn!(
                            block_number,
                            oldest_missing_entry = number,
                            "missing CanonicalHash entries; falling back to embedded block hashes"
                        );
                        self.get_legacy_context(key)
                            .expect("canonical rows are dual-written; legacy copy must exist")
                            .block_hashes
                    });
                    embedded.0[index]
                }
            };
        }
        BlockHashes(hashes)
    }

    fn get_stored_context_v2(&self, key: &[u8]) -> Option<StoredBlockContextV2> {
        self.db
            .get_cf(BlockReplayColumnFamily::ContextV2, key)
            .expect("Cannot read from DB")
            .map(|bytes| {
                bincode::decode_from_slice(&bytes, bincode::config::standard())
                    .expect("Failed to deserialize stripped context")
                    .0
            })
    }

    fn get_legacy_context(&self, key: &[u8]) -> Option<BlockContext> {
        self.db
            .get_cf(BlockReplayColumnFamily::Context, key)
            .expect("Cannot read from DB")
            .map(|bytes| {
                bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
                    .expect("Failed to deserialize context")
                    .0
            })
    }

    fn get_context_by_key(&self, block_number: BlockNumber, key: &[u8]) -> Option<BlockContext> {
        // Prefer the stripped row: its block hashes are reconstructed from `CanonicalHash`, so
        // nothing depends on the copy embedded in the legacy row anymore.
        if let Some(stored) = self.get_stored_context_v2(key) {
            assert_eq!(
                stored.block_number, block_number,
                "block number mismatch when reading context"
            );
            return Some(
                stored.into_context(self.chain_id, self.read_block_hashes(block_number, key)),
            );
        }
        // No stripped row: either a canonical row written before `ContextV2` existed (or by an
        // older binary during a rollback), or a non-canonical (hash-keyed) row holding a
        // reverted block. Both are served as stored: for the former the embedded hashes are
        // just as correct, and for the latter they are authoritative — `CanonicalHash` entries
        // for a reverted block's ancestors may have been overwritten by the new canonical chain.
        self.get_legacy_context(key)
    }

    /// Cheaper than [`ReadReplay::get_context`] when only the timestamp is needed: skips
    /// reconstructing the 256 block hashes (and, for stripped rows, decoding them).
    fn get_block_timestamp(&self, block_number: BlockNumber) -> Option<u64> {
        let key = block_number.to_be_bytes();
        self.get_stored_context_v2(&key)
            .map(|context| context.timestamp)
            .or_else(|| {
                self.get_legacy_context(&key)
                    .map(|context| context.timestamp)
            })
    }
}

impl ReadReplay for BlockReplayStorage {
    fn get_context(&self, block_number: BlockNumber) -> Option<BlockContext> {
        self.get_context_by_key(block_number, &block_number.to_be_bytes())
    }

    fn get_replay_record_by_key(
        &self,
        block_number: u64,
        db_key: Option<Vec<u8>>,
    ) -> Option<ReplayRecord> {
        let key = db_key.unwrap_or_else(|| block_number.to_be_bytes().to_vec());
        // Writes are atomic, so if we can't read the context, we can't read the rest of the
        // replay record anyway.
        let block_context = self.get_context_by_key(block_number, &key)?;
        Some(self.assemble_replay_record(block_number, &key, block_context))
    }

    fn latest_record(&self) -> BlockNumber {
        // This is guaranteed to be non-`None` because genesis is always inserted on storage initialization.
        self.latest_record_checked()
            .expect("no blocks in BlockReplayStorage")
    }
}

impl BlockReplayStorage {
    /// Like [`ReadReplay::get_replay_record`], but takes the block context from the legacy row,
    /// embedded hashes included. Used by the override path: during a multi-block override the
    /// `CanonicalHash` entries of already-overridden ancestors point at the new chain, so
    /// reconstructing the hashes would splice new-chain entries into the record being archived.
    fn get_replay_record_with_original_hashes(
        &self,
        block_number: BlockNumber,
    ) -> Option<ReplayRecord> {
        let key = block_number.to_be_bytes();
        let Some(block_context) = self.get_legacy_context(&key) else {
            // The legacy copy exists for everything this binary writes; it can only be missing
            // after rolling back from a version whose migration deleted it. Reconstruction keeps
            // override writes (e.g. an EN re-writing blocks on restart) working in that state;
            // it is only inexact for an in-flight multi-block override wave, which a rolled-back
            // binary should not be running.
            return self.get_replay_record(block_number);
        };
        Some(self.assemble_replay_record(block_number, &key, block_context))
    }

    /// Assembles a [`ReplayRecord`] around an already-resolved context.
    ///
    /// Writes are atomic and a context for this key exists, so the rest of the replay record
    /// must be present too. Hence, we can safely unwrap here.
    fn assemble_replay_record(
        &self,
        block_number: u64,
        key: &[u8],
        block_context: BlockContext,
    ) -> ReplayRecord {
        let starting_l1_priority_id = self
            .db
            .get_cf(BlockReplayColumnFamily::StartingL1SerialId, key)
            .expect("Failed to read from LastProcessedL1TxId CF")
            .expect("StartingL1SerialId must be written atomically with Context");
        let transactions = self
            .db
            .get_cf(BlockReplayColumnFamily::Txs, key)
            .expect("Failed to read from Txs CF")
            .expect("Txs must be written atomically with Context");
        // todo: save `previous_block_timestamp` as another column in the next breaking change to
        //       replay record format
        let previous_block_timestamp = if block_number == 0 {
            // Genesis does not have previous block and this value should never be used, but we
            // return `0` here for the flow to work.
            0
        } else {
            self.get_block_timestamp(block_number - 1).unwrap_or(0)
        };

        let node_version = self
            .db
            .get_cf(BlockReplayColumnFamily::NodeVersion, key)
            .expect("Failed to read from NodeVersion CF")
            .expect("NodeVersion must be written atomically with Context");

        let protocol_version = if let Some(version) = self
            .db
            .get_cf(BlockReplayColumnFamily::ProtocolVersion, key)
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
            .get_cf(BlockReplayColumnFamily::ForcePreimages, key)
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
            .get_cf(BlockReplayColumnFamily::BlockOutputHash, key)
            .expect("Failed to read from BlockOutputHash CF")
            .expect("BlockOutputHash must be written atomically with Context");

        let starting_interop_root_id = if let Some(starting_interop_root_id) = self
            .db
            .get_cf(BlockReplayColumnFamily::StartingInteropRootId, key)
            .expect("Failed to read from StartingInteropRootId CF")
        {
            let stored: u64 = bincode::serde::decode_from_slice(
                &starting_interop_root_id,
                bincode::config::standard(),
            )
            .expect("Failed to deserialize starting interop root id")
            .0;
            stored
        } else {
            0
        };

        let starting_migration_number = if let Some(starting_migration_number) = self
            .db
            .get_cf(BlockReplayColumnFamily::StartingMigrationNumber, key)
            .expect("Failed to read from StartingMigrationNumber CF")
        {
            let stored: u64 = bincode::serde::decode_from_slice(
                &starting_migration_number,
                bincode::config::standard(),
            )
            .expect("Failed to deserialize starting migration number")
            .0;
            stored
        } else {
            0
        };

        let starting_interop_fee_number = if let Some(starting_interop_fee_number) = self
            .db
            .get_cf(BlockReplayColumnFamily::StartingInteropFeeNumber, key)
            .expect("Failed to read from StartingInteropFeeNumber CF")
        {
            let stored: u64 = bincode::serde::decode_from_slice(
                &starting_interop_fee_number,
                bincode::config::standard(),
            )
            .expect("Failed to deserialize starting interop fee number")
            .0;
            stored
        } else {
            0
        };

        // TODO(RocksDB migration): BlockStartCursors fields are reassembled from separate column
        // families below. A future migration should read them from a single CF.
        ReplayRecord {
            block_context,
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
            starting_cursors: BlockStartCursors {
                l1_priority_id: bincode::serde::decode_from_slice(
                    &starting_l1_priority_id,
                    bincode::config::standard(),
                )
                .expect("Failed to deserialize starting_l1_priority_id")
                .0,
                interop_root_id: starting_interop_root_id,
                migration_number: starting_migration_number,
                interop_fee_number: starting_interop_fee_number,
            },
        }
    }
}

impl WriteReplay for BlockReplayStorage {
    async fn write(
        &self,
        sealed_record: Sealed<ReplayRecord>,
        override_allowed: bool,
    ) -> anyhow::Result<bool> {
        let latency_observer = BLOCK_REPLAY_ROCKS_DB_METRICS.get_latency.start();
        let block_record = sealed_record.as_ref();
        let block_context = &sealed_record.block_context;
        let current_latest_record = self.latest_record_checked();
        let Some(current_latest_record) = current_latest_record else {
            assert_eq!(
                block_context.block_number, 0,
                "tried to append first replay record with non-zero block number: {}",
                block_context.block_number
            );
            self.write_replay_unchecked(sealed_record, true);
            latency_observer.observe();
            return Ok(true);
        };

        if block_context.block_number <= current_latest_record && !override_allowed {
            // todo: consider asserting that the passed `ReplayRecord` matches the one currently stored
            tracing::debug!(
                block_number = block_context.block_number,
                "not appending block: already exists in block replay storage",
            );
            return Ok(false);
        } else if block_context.block_number > current_latest_record + 1 {
            panic!(
                "tried to append non-sequential replay record: {} > {}",
                block_context.block_number,
                current_latest_record + 1
            );
        }

        if block_context.block_number <= current_latest_record {
            // The legacy copy always exists for canonical rows (they are dual-written), and it
            // is the only correct hash source here: reconstruction would splice new-chain
            // hashes into the archived record when overriding a range of blocks.
            let old_record = self
                .get_replay_record_with_original_hashes(block_context.block_number)
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
        Ok(true)
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

/// [`BlockContext`] as persisted in the `ContextV2` CF: without the 256 previous block hashes
/// (derivable from the `CanonicalHash` CF, ~8 KiB per block; see
/// [`BlockReplayStorage::read_block_hashes`]) and without the chain id (shared by all blocks, not
/// persisted at all — supplied by [`BlockReplayStorage`]'s in-memory `chain_id` field on read).
///
/// The exhaustive destructuring in the conversions below keeps this struct in sync with
/// [`BlockContext`]: a field added there fails compilation here, prompting a decision on whether
/// and how to persist it.
#[derive(Debug, bincode::Encode, bincode::Decode)]
struct StoredBlockContextV2 {
    block_number: u64,
    timestamp: u64,
    #[bincode(with_serde)]
    eip1559_basefee: U256,
    #[bincode(with_serde)]
    pubdata_price: U256,
    #[bincode(with_serde)]
    native_price: U256,
    #[bincode(with_serde)]
    coinbase: Address,
    gas_limit: u64,
    pubdata_limit: u64,
    #[bincode(with_serde)]
    mix_hash: U256,
    execution_version: u32,
    #[bincode(with_serde)]
    blob_fee: U256,
}

impl StoredBlockContextV2 {
    fn strip(context: BlockContext) -> Self {
        let BlockContext {
            chain_id: _,
            block_number,
            block_hashes: _,
            timestamp,
            eip1559_basefee,
            pubdata_price,
            native_price,
            coinbase,
            gas_limit,
            pubdata_limit,
            mix_hash,
            execution_version,
            blob_fee,
        } = context;
        Self {
            block_number,
            timestamp,
            eip1559_basefee,
            pubdata_price,
            native_price,
            coinbase,
            gas_limit,
            pubdata_limit,
            mix_hash,
            execution_version,
            blob_fee,
        }
    }

    fn into_context(self, chain_id: u64, block_hashes: BlockHashes) -> BlockContext {
        let Self {
            block_number,
            timestamp,
            eip1559_basefee,
            pubdata_price,
            native_price,
            coinbase,
            gas_limit,
            pubdata_limit,
            mix_hash,
            execution_version,
            blob_fee,
        } = self;
        BlockContext {
            chain_id,
            block_number,
            block_hashes,
            timestamp,
            eip1559_basefee,
            pubdata_price,
            native_price,
            coinbase,
            gas_limit,
            pubdata_limit,
            mix_hash,
            execution_version,
            blob_fee,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zksync_os_types::BlockStartCursors;

    fn fake_hash(seed: u64) -> BlockHash {
        BlockHash::from(U256::from(0xFF_0000 + seed))
    }

    /// Builds a chain of sealed replay records with properly chained `block_hashes`,
    /// mirroring how `BlockContextProvider` advances the window in memory.
    fn make_chain(len: u64) -> Vec<Sealed<ReplayRecord>> {
        let mut chain: Vec<Sealed<ReplayRecord>> = Vec::new();
        for number in 0..len {
            let (block_hashes, previous_block_timestamp) = match chain.last() {
                Some(previous) => (
                    previous.block_context.block_hashes.push(previous.hash()),
                    previous.block_context.timestamp,
                ),
                None => (BlockHashes::default(), 0),
            };
            let context = BlockContext {
                chain_id: 270,
                block_number: number,
                block_hashes,
                timestamp: 1_000 + number,
                gas_limit: 100_000_000,
                ..Default::default()
            };
            let record = ReplayRecord {
                block_context: context,
                transactions: vec![],
                previous_block_timestamp,
                node_version: NODE_SEMVER_VERSION.clone(),
                protocol_version: ProtocolSemanticVersion::legacy_genesis_version(),
                block_output_hash: B256::from(U256::from(0xB0_0000 + number)),
                force_preimages: vec![],
                starting_cursors: BlockStartCursors::default(),
            };
            chain.push(Sealed::new_unchecked(record, fake_hash(number)));
        }
        chain
    }

    /// Overwrites the embedded block hashes of a stored canonical row with garbage.
    fn corrupt_embedded_hashes(storage: &BlockReplayStorage, record: &ReplayRecord) {
        let mut context = record.block_context;
        context.block_hashes = BlockHashes([U256::from(0xDEAD_BEEF_u64); 256]);
        let db_key = context.block_number.to_be_bytes();
        let context_value =
            bincode::serde::encode_to_vec(context, bincode::config::standard()).unwrap();
        let mut batch = storage.db.new_write_batch();
        batch.put_cf(BlockReplayColumnFamily::Context, &db_key, &context_value);
        storage.db.write(batch).unwrap();
    }

    /// Simulates rows written before the `CanonicalHash` CF existed.
    fn delete_canonical_hash_entries(
        storage: &BlockReplayStorage,
        blocks: std::ops::RangeInclusive<u64>,
    ) {
        let mut batch = storage.db.new_write_batch();
        for number in blocks {
            batch.delete_cf(
                BlockReplayColumnFamily::CanonicalHash,
                &number.to_be_bytes(),
            );
        }
        storage.db.write(batch).unwrap();
    }

    /// Simulates rows written before `ContextV2` existed (or by an older binary during a
    /// rollback): only the full legacy row is present.
    fn delete_stripped_rows(storage: &BlockReplayStorage, blocks: std::ops::RangeInclusive<u64>) {
        let mut batch = storage.db.new_write_batch();
        for number in blocks {
            batch.delete_cf(BlockReplayColumnFamily::ContextV2, &number.to_be_bytes());
        }
        storage.db.write(batch).unwrap();
    }

    fn assert_chain_reads_back(storage: &BlockReplayStorage, chain: &[Sealed<ReplayRecord>]) {
        for sealed in chain {
            let number = sealed.block_context.block_number;
            assert_eq!(
                storage.get_context(number).as_ref(),
                Some(&sealed.block_context),
                "context mismatch for block {number}"
            );
            assert_eq!(
                storage.get_replay_record(number).as_ref(),
                Some(sealed.as_ref()),
                "record mismatch for block {number}"
            );
        }
    }

    #[tokio::test]
    async fn reconstructs_hashes_from_canonical_hash_cf() {
        let dir = tempfile::tempdir().unwrap();
        let storage = BlockReplayStorage::new_without_genesis(dir.path(), 270);
        // Crosses the 256-hash window boundary: early blocks are zero-padded, later ones aren't.
        let chain = make_chain(300);
        for sealed in &chain {
            assert!(storage.write(sealed.clone(), false).await.unwrap());
        }
        assert_eq!(storage.latest_record(), 299);
        assert_chain_reads_back(&storage, &chain);

        // Canonical reads must not depend on the embedded hashes: corrupt them and verify the
        // rows still read back correctly, reconstructed from `CanonicalHash`.
        for number in [0, 1, 5, 100, 257, 299] {
            corrupt_embedded_hashes(&storage, chain[number].as_ref());
        }
        assert_chain_reads_back(&storage, &chain);
    }

    #[tokio::test]
    async fn falls_back_to_embedded_hashes_when_canonical_hash_missing() {
        let dir = tempfile::tempdir().unwrap();
        let storage = BlockReplayStorage::new_without_genesis(dir.path(), 270);
        let chain = make_chain(300);
        for sealed in &chain {
            storage.write(sealed.clone(), false).await.unwrap();
        }
        // Blocks 0..=150 simulate rows written before the `CanonicalHash` CF existed: later
        // blocks' windows overlap them, mixing reconstructed and embedded-fallback entries.
        delete_canonical_hash_entries(&storage, 0..=150);
        assert_chain_reads_back(&storage, &chain);
    }

    #[tokio::test]
    async fn rows_without_stripped_copy_are_served_from_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let storage = BlockReplayStorage::new_without_genesis(dir.path(), 270);
        let chain = make_chain(300);
        for sealed in &chain {
            storage.write(sealed.clone(), false).await.unwrap();
        }
        delete_stripped_rows(&storage, 100..=200);
        assert_chain_reads_back(&storage, &chain);
    }

    #[tokio::test]
    async fn override_keeps_old_record_readable_by_hash() {
        let dir = tempfile::tempdir().unwrap();
        let storage = BlockReplayStorage::new_without_genesis(dir.path(), 270);
        let chain = make_chain(5);
        for sealed in &chain {
            storage.write(sealed.clone(), false).await.unwrap();
        }

        let old = &chain[4];
        let mut replacement = old.as_ref().clone();
        replacement.block_context.timestamp += 100;
        let replacement_hash = fake_hash(999);
        assert!(
            storage
                .write(
                    Sealed::new_unchecked(replacement.clone(), replacement_hash),
                    true
                )
                .await
                .unwrap()
        );

        // The replaced record stays readable under its hash key, embedded hashes intact.
        // Non-canonical rows are never dual-written: only the legacy row exists for them.
        assert!(storage.get_stored_context_v2(&old.hash().0).is_none());
        assert_eq!(
            storage
                .get_replay_record_by_key(4, Some(old.hash().0.to_vec()))
                .as_ref(),
            Some(old.as_ref())
        );
        // The canonical row now returns the replacement.
        assert_eq!(storage.get_replay_record(4), Some(replacement.clone()));

        // A subsequent block chains from the replacement's hash via reconstruction.
        let mut next = replacement.clone();
        next.block_context.block_number = 5;
        next.block_context.block_hashes = replacement
            .block_context
            .block_hashes
            .push(replacement_hash);
        next.previous_block_timestamp = replacement.block_context.timestamp;
        storage
            .write(Sealed::new_unchecked(next.clone(), fake_hash(5)), false)
            .await
            .unwrap();
        assert_eq!(storage.get_replay_record(5), Some(next));
    }

    #[tokio::test]
    async fn override_write_tolerates_contracted_rows() {
        // After the contract migration (next release) deletes legacy rows, a rollback to this
        // binary must still handle override writes: an EN re-writes existing blocks on restart.
        let dir = tempfile::tempdir().unwrap();
        let storage = BlockReplayStorage::new_without_genesis(dir.path(), 270);
        let chain = make_chain(5);
        for sealed in &chain {
            storage.write(sealed.clone(), false).await.unwrap();
        }
        // Contract block 3: the stripped row and `CanonicalHash` stay, the legacy row goes.
        let mut batch = storage.db.new_write_batch();
        batch.delete_cf(BlockReplayColumnFamily::Context, &3u64.to_be_bytes());
        storage.db.write(batch).unwrap();

        // An identical re-write proceeds without panicking or archiving anything.
        assert!(storage.write(chain[3].clone(), true).await.unwrap());
        assert_eq!(
            storage.get_replay_record(3).as_ref(),
            Some(chain[3].as_ref())
        );
    }

    #[tokio::test]
    async fn multi_block_override_archives_original_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let storage = BlockReplayStorage::new_without_genesis(dir.path(), 270);
        let chain = make_chain(5);
        for sealed in &chain {
            storage.write(sealed.clone(), false).await.unwrap();
        }

        // Replace blocks 3 and 4 sequentially, as a range rebuild does. By the time block 4 is
        // overridden, `CanonicalHash[3]` already points at the new chain, so its archived copy
        // must not be reconstructed from the index.
        let (old3, old4) = (&chain[3], &chain[4]);
        let mut new3 = old3.as_ref().clone();
        new3.block_context.timestamp += 100;
        let new3_hash = fake_hash(103);
        storage
            .write(Sealed::new_unchecked(new3.clone(), new3_hash), true)
            .await
            .unwrap();
        let mut new4 = old4.as_ref().clone();
        new4.block_context.timestamp += 100;
        new4.block_context.block_hashes = new3.block_context.block_hashes.push(new3_hash);
        new4.previous_block_timestamp = new3.block_context.timestamp;
        storage
            .write(Sealed::new_unchecked(new4.clone(), fake_hash(104)), true)
            .await
            .unwrap();

        // The archived copies keep the hashes they were executed with: old block 4's window
        // ends with old block 3's hash, not the replacement's. (Contexts are compared instead
        // of full records because `previous_block_timestamp` of archived rows is derived from
        // the current canonical neighbor on read — a pre-existing quirk.)
        let old4_read = storage
            .get_replay_record_by_key(4, Some(old4.hash().0.to_vec()))
            .unwrap();
        assert_eq!(old4_read.block_context, old4.block_context);
        let old3_read = storage
            .get_replay_record_by_key(3, Some(old3.hash().0.to_vec()))
            .unwrap();
        assert_eq!(old3_read.block_context, old3.block_context);
        // The new canonical rows reconstruct against the repointed index.
        assert_eq!(storage.get_replay_record(3), Some(new3));
        assert_eq!(storage.get_replay_record(4), Some(new4));
    }
}
