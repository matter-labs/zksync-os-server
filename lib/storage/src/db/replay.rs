use alloy::primitives::{Address, B256, BlockHash, BlockNumber, Sealed, U256};
use anyhow::Context as _;
use std::convert::TryInto;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vise::Unit;
use vise::{Buckets, Histogram, Metrics};
use zksync_os_genesis::Genesis;
use zksync_os_metadata::NODE_SEMVER_VERSION;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::{NamedColumnFamily, WriteBatch};
use zksync_os_rocksdb::migrations::Migration;
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
    /// The block number and original hash of the row most recently archived by an override, if
    /// any override has happened since the last plain append. Bridges the single moment, within
    /// a multi-block override wave, between overwriting block `N`'s canonical row and archiving
    /// block `N+1`'s: at that point `CanonicalHash[N]` already points at `N`'s replacement, so
    /// `N+1`'s original parent hash — needed to archive it — can no longer be read live and must
    /// come from here instead. Cleared on plain appends and on a non-consecutive override (a new,
    /// unrelated wave, whose first member's parent hash is still safe to read live).
    ///
    /// Unlike the hash window and `previous_block_timestamp`, which [`ArchivedContext`] rows
    /// resolve durably by walking `parent_hash` pointers (see
    /// [`Self::resolve_window_and_previous_timestamp`]), this one hash genuinely has no other
    /// source once its slot is overwritten — no row (canonical or archived) records its own
    /// parent hash independently of the live `CanonicalHash` index. In-memory only: a crash
    /// mid-wave loses it, so the archived copy of the rest of a resumed wave gets its parent hash
    /// from `CanonicalHash` instead, which by then points at the new chain — a narrower version
    /// of the same caveat the old `displaced_hashes` map had. Canonical data is unaffected.
    last_archived: Arc<Mutex<Option<(BlockNumber, BlockHash)>>>,
}

/// Column families for storage of block replay commands.
///
/// TODO(RocksDB migration): The four `Starting*` column families below correspond to fields
/// in [`BlockStartCursors`]. They are stored separately for historical reasons (each was added
/// independently). A future migration should consolidate them into a single column family
/// serializing the entire `BlockStartCursors` struct.
#[derive(Copy, Clone, Debug)]
pub enum BlockReplayColumnFamily {
    /// Full [`BlockContext`], including the 256 previous block hashes (~8 KiB per block). Holds
    /// hash-keyed archived rows written before [`Self::ArchivedContext`] existed. Number-keyed
    /// rows only appear here when written by binaries predating [`Self::ContextV2`]; the
    /// [`ContractBlockContexts`] migration converts and deletes them. Never written to going
    /// forward; new archived rows go to [`Self::ArchivedContext`] instead.
    Context,
    /// Stripped [`BlockContext`] for canonical rows: everything except the 256 previous block
    /// hashes, which are derivable data and get reconstructed from [`Self::CanonicalHash`] on
    /// read (see [`StoredBlockContextV2`]).
    ContextV2,
    /// Hash-keyed archived rows (blocks displaced by an override), holding [`StoredBlockContextV2`]
    /// plus the row's own `parent_hash` instead of the full hash window. Reconstructing the
    /// window (and the row's `previous_block_timestamp`) means walking `parent_hash` pointers
    /// through this same CF until reaching a hash that isn't archived — i.e. one still part of
    /// the live canonical chain — then finishing with a `CanonicalHash` lookup (see
    /// [`BlockReplayStorage::resolve_window_and_previous_timestamp`]). Unlike [`Self::Context`],
    /// this never depends on the live `CanonicalHash` index staying unchanged, so it stays correct
    /// no matter how long after an override wave, or in how different a process, it is read.
    ArchivedContext,
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
        BlockReplayColumnFamily::ArchivedContext,
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
            BlockReplayColumnFamily::ArchivedContext => "archived_context",
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
            .with_sync_writes()
            .run_migrations(MIGRATIONS)
            .expect("replay WAL schema migration failed");

        let this = Self {
            db,
            chain_id: genesis.chain_id(),
            last_archived: Arc::default(),
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
            this.write_replay_unchecked(sealed_genesis_record.clone(), WriteKind::Canonical);
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
            .with_sync_writes()
            .run_migrations(MIGRATIONS)
            .expect("replay WAL schema migration failed");
        Self {
            db,
            chain_id,
            last_archived: Arc::default(),
        }
    }

    fn write_replay_unchecked(&self, sealed_record: Sealed<ReplayRecord>, kind: WriteKind) {
        // Prepare record
        let (record, block_hash) = sealed_record.split();
        // TODO: We want to change the key to be block_hash for all blocks
        let db_key = match kind {
            WriteKind::Canonical => record.block_context.block_number.to_be_bytes().to_vec(),
            WriteKind::Archived { .. } => block_hash.0.to_vec(),
        };
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
        match kind {
            WriteKind::Canonical => {
                // Canonical rows don't persist the 256 previous block hashes: they are derivable
                // from `CanonicalHash` and get reconstructed on read.
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
            WriteKind::Archived { parent_hash } => {
                // Archived rows keep their own `parent_hash` instead of the full hash window:
                // the window (and `previous_block_timestamp`) are reconstructed on read by
                // walking `parent_hash` pointers (see
                // `resolve_window_and_previous_timestamp`), which stays correct regardless of
                // what happens to `CanonicalHash` afterwards.
                let archived_value = bincode::encode_to_vec(
                    StoredArchivedContext {
                        parent_hash,
                        stripped: StoredBlockContextV2::strip(record.block_context),
                    },
                    bincode::config::standard(),
                )
                .expect("Failed to serialize archived record.context");
                batch.put_cf(
                    BlockReplayColumnFamily::ArchivedContext,
                    &db_key,
                    &archived_value,
                );
            }
        }
        if self
            .latest_record_checked()
            .is_none_or(|l| l < record.block_context.block_number)
        {
            batch.put_cf(BlockReplayColumnFamily::Latest, Self::LATEST_KEY, &db_key);
        }
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
        // Complete for all canonical blocks: backfilled by [`ContractBlockContexts`] and written
        // with every canonical row since.
        self.get_canonical_block_hash_opt(block_number)
            .unwrap_or_else(|| panic!("no CanonicalHash entry for block {block_number}"))
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

    /// Reconstructs the 256 previous block hashes for the canonical block at `block_number` from
    /// `CanonicalHash`. The hash of block `M` lands at index `M + 256 - block_number` (most
    /// recent last); slots before genesis stay zero, mirroring how the genesis context is
    /// initialized. Always correct for a canonical row: unlike an archived row, it's never
    /// disconnected from the live index it's read through.
    fn read_block_hashes(&self, block_number: BlockNumber) -> BlockHashes {
        let mut hashes = [U256::ZERO; 256];
        let first = block_number.saturating_sub(256);
        let keys = (first..block_number)
            .map(|number| number.to_be_bytes())
            .collect::<Vec<_>>();
        let results = self
            .db
            .multi_get_cf(BlockReplayColumnFamily::CanonicalHash, keys.iter());
        for (number, result) in (first..block_number).zip(results) {
            let index = (number + 256 - block_number) as usize;
            hashes[index] = match result.expect("Failed to read from CanonicalHash DB") {
                Some(bytes) => U256::from_be_slice(&bytes),
                None => panic!("no CanonicalHash entry for block {number}"),
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

    fn get_archived(&self, key: &[u8]) -> Option<StoredArchivedContext> {
        self.db
            .get_cf(BlockReplayColumnFamily::ArchivedContext, key)
            .expect("Cannot read from DB")
            .map(|bytes| {
                bincode::decode_from_slice(&bytes, bincode::config::standard())
                    .expect("Failed to deserialize archived context")
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

    /// Reconstructs the 256-hash window and `previous_block_timestamp` for a row whose immediate
    /// parent is `parent_hash`, by walking `parent_hash` pointers through `ArchivedContext`.
    ///
    /// The walk stops as soon as it reaches a hash that isn't itself archived — i.e. one still
    /// part of the live canonical chain. From that point on `CanonicalHash` is guaranteed
    /// unmutated (overrides only ever touch a wave's own consecutive range, and this hash is
    /// outside it), so the rest of the window is filled with one batched lookup instead of
    /// walking further. This is what lets an archived row be read correctly no matter how long
    /// after the override, or in how different a process, the read happens — unlike the in-memory
    /// `last_archived` cache used at archival time, this has no other state to lose.
    fn resolve_window_and_previous_timestamp(
        &self,
        block_number: BlockNumber,
        parent_hash: BlockHash,
    ) -> (BlockHashes, u64) {
        if block_number == 0 {
            return (BlockHashes::default(), 0);
        }
        let mut hashes = [U256::ZERO; 256];
        let first = block_number.saturating_sub(256);
        let total = (block_number - first) as usize;
        let mut current_hash = parent_hash;
        let mut previous_block_timestamp = 0;
        for step in 0..total {
            hashes[255 - step] = U256::from_be_bytes(current_hash.0);
            match self.get_archived(current_hash.0.as_slice()) {
                Some(archived) => {
                    if step == 0 {
                        previous_block_timestamp = archived.stripped.timestamp;
                    }
                    if step + 1 == total {
                        break;
                    }
                    current_hash = archived.parent_hash;
                }
                None => {
                    // `current_hash` is still part of the live canonical chain.
                    let this_number = block_number - 1 - step as u64;
                    if step == 0 {
                        previous_block_timestamp = self
                            .get_block_timestamp(this_number)
                            .expect("current canonical row must have a timestamp");
                    }
                    if this_number > first {
                        let keys = (first..this_number)
                            .map(|number| number.to_be_bytes())
                            .collect::<Vec<_>>();
                        let results = self
                            .db
                            .multi_get_cf(BlockReplayColumnFamily::CanonicalHash, keys.iter());
                        for (number, result) in (first..this_number).zip(results) {
                            let index = (number + 256 - block_number) as usize;
                            hashes[index] =
                                match result.expect("Failed to read from CanonicalHash DB") {
                                    Some(bytes) => U256::from_be_slice(&bytes),
                                    None => panic!("no CanonicalHash entry for block {number}"),
                                };
                        }
                    }
                    break;
                }
            }
        }
        (BlockHashes(hashes), previous_block_timestamp)
    }

    /// Resolves a context by key, and the `previous_block_timestamp` to go with it. Checks
    /// `ContextV2` (canonical), then `ArchivedContext` (a row archived by this or an earlier
    /// binary), then `Context` (a row predating one of the two stripped formats).
    fn resolve_context(
        &self,
        block_number: BlockNumber,
        key: &[u8],
    ) -> Option<(BlockContext, u64)> {
        if let Some(stored) = self.get_stored_context_v2(key) {
            assert_eq!(
                stored.block_number, block_number,
                "block number mismatch when reading context"
            );
            let previous_block_timestamp = if block_number == 0 {
                0
            } else {
                self.get_block_timestamp(block_number - 1).unwrap_or(0)
            };
            return Some((
                stored.into_context(self.chain_id, self.read_block_hashes(block_number)),
                previous_block_timestamp,
            ));
        }
        if let Some(archived) = self.get_archived(key) {
            let (block_hashes, previous_block_timestamp) =
                self.resolve_window_and_previous_timestamp(block_number, archived.parent_hash);
            return Some((
                archived.stripped.into_context(self.chain_id, block_hashes),
                previous_block_timestamp,
            ));
        }
        // No stripped row: either a canonical row written before `ContextV2` existed (or by an
        // older binary during a rollback), or an archived row written before `ArchivedContext`
        // existed. Both are served as stored: for the former the embedded hashes are just as
        // correct, and for the latter they are authoritative — but `previous_block_timestamp` is
        // derived from the current canonical neighbor either way, which is only exactly right
        // for the former (a pre-existing quirk for this legacy archived-row shape; new archived
        // rows resolve it durably above).
        let context = self.get_legacy_context(key)?;
        let previous_block_timestamp = if block_number == 0 {
            0
        } else {
            self.get_block_timestamp(block_number - 1).unwrap_or(0)
        };
        Some((context, previous_block_timestamp))
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
        self.resolve_context(block_number, &block_number.to_be_bytes())
            .map(|(context, _)| context)
    }

    fn get_replay_record_by_key(
        &self,
        block_number: u64,
        db_key: Option<Vec<u8>>,
    ) -> Option<ReplayRecord> {
        let key = db_key.unwrap_or_else(|| block_number.to_be_bytes().to_vec());
        // Writes are atomic, so if we can't read the context, we can't read the rest of the
        // replay record anyway.
        let (block_context, previous_block_timestamp) = self.resolve_context(block_number, &key)?;
        Some(self.assemble_replay_record(
            block_number,
            &key,
            block_context,
            previous_block_timestamp,
        ))
    }

    fn latest_record(&self) -> BlockNumber {
        // This is guaranteed to be non-`None` because genesis is always inserted on storage initialization.
        self.latest_record_checked()
            .expect("no blocks in BlockReplayStorage")
    }
}

impl BlockReplayStorage {
    /// Like [`ReadReplay::get_replay_record`], but resolves the hash window and
    /// `previous_block_timestamp` from `parent_hash` instead of the live canonical chain. Used
    /// when archiving a row that is about to be overridden: if an earlier override in the same
    /// wave already replaced this row's parent, the live chain no longer has the parent this row
    /// was actually executed against, so the caller must supply it explicitly (see
    /// [`WriteReplay::write`](Self)'s `last_archived`).
    fn get_replay_record_with_original_parent(
        &self,
        block_number: BlockNumber,
        parent_hash: BlockHash,
    ) -> Option<ReplayRecord> {
        let key = block_number.to_be_bytes();
        let (block_context, previous_block_timestamp) =
            if let Some(stored) = self.get_stored_context_v2(&key) {
                let (block_hashes, previous_block_timestamp) =
                    self.resolve_window_and_previous_timestamp(block_number, parent_hash);
                (
                    stored.into_context(self.chain_id, block_hashes),
                    previous_block_timestamp,
                )
            } else {
                // Rows written by binaries predating the stripped format (only reachable by rolling
                // back further than supported); their embedded hashes are already the originals.
                let context = self.get_legacy_context(&key)?;
                let previous_block_timestamp = if block_number == 0 {
                    0
                } else {
                    self.get_block_timestamp(block_number - 1).unwrap_or(0)
                };
                (context, previous_block_timestamp)
            };
        Some(self.assemble_replay_record(
            block_number,
            &key,
            block_context,
            previous_block_timestamp,
        ))
    }

    /// Assembles a [`ReplayRecord`] around an already-resolved context and
    /// `previous_block_timestamp`.
    ///
    /// Writes are atomic and a context for this key exists, so the rest of the replay record
    /// must be present too. Hence, we can safely unwrap here.
    fn assemble_replay_record(
        &self,
        block_number: u64,
        key: &[u8],
        block_context: BlockContext,
        previous_block_timestamp: u64,
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
            self.write_replay_unchecked(sealed_record, WriteKind::Canonical);
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
            let block_number = block_context.block_number;
            let mut last_archived = self.last_archived.lock().unwrap();
            // The original parent hash of the row about to be overridden. Usually still safe to
            // read live from `CanonicalHash`— unless the immediately preceding block was itself
            // overridden earlier in this same wave, in which case that already points at its
            // replacement, and the true original parent is only available from `last_archived`.
            let parent_hash = if block_number == 0 {
                // Unused: genesis has no parent, and the resolvers below short-circuit on
                // `block_number == 0` without consulting it.
                BlockHash::ZERO
            } else {
                match *last_archived {
                    Some((last_number, last_hash)) if last_number + 1 == block_number => last_hash,
                    _ => self.get_canonical_block_hash(block_number - 1),
                }
            };
            let old_record = self
                .get_replay_record_with_original_parent(block_number, parent_hash)
                .expect("Old record must exist");
            if &old_record != block_record {
                let old_record_hash = self.get_canonical_block_hash(block_number);
                let old_record_hash_hex = alloy::hex::encode_prefixed(old_record_hash.0);
                tracing::warn!(
                    block_number,
                    old_record_hash_hex,
                    "Overriding existing block replay record",
                );
                self.write_replay_unchecked(
                    Sealed::new_unchecked(old_record, old_record_hash),
                    WriteKind::Archived { parent_hash },
                );
                *last_archived = Some((block_number, old_record_hash));
            }
        } else {
            // A plain append means whatever chain the last wave produced is final; a future
            // override at these heights is built on it, not on the cached parent hash.
            *self.last_archived.lock().unwrap() = None;
        }

        self.write_replay_unchecked(sealed_record, WriteKind::Canonical);
        latency_observer.observe();
        Ok(true)
    }
}

/// How [`BlockReplayStorage::write_replay_unchecked`] should persist a row.
#[derive(Clone, Copy, Debug)]
enum WriteKind {
    /// A canonical (number-keyed) row: stripped of the hash window, indexed in `CanonicalHash`.
    Canonical,
    /// A row displaced by an override (hash-keyed): stripped of the hash window in favor of its
    /// own `parent_hash`, from which the window is reconstructed on read.
    Archived { parent_hash: BlockHash },
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

/// Migrations of the replay WAL, run by [`BlockReplayStorage`]'s constructors before the
/// database serves anything.
static MIGRATIONS: &[&dyn Migration<BlockReplayColumnFamily>] = &[&ContractBlockContexts];

/// Contract phase of dropping the persisted 256 block hashes: for every canonical row, ensures
/// the `CanonicalHash` entry and the stripped `ContextV2` row exist, then deletes the legacy
/// full-context row (~8 KiB per block). Non-canonical (hash-keyed) rows are left untouched;
/// only number keys are visited, so the two can never be confused. Ends with a compaction of the
/// legacy CF so the tombstoned space is actually reclaimed.
///
/// Idempotent: each per-row step is skipped when already done, and every 1000-row chunk commits
/// atomically, so a crash mid-run resumes cleanly on the next open.
struct ContractBlockContexts;

impl Migration<BlockReplayColumnFamily> for ContractBlockContexts {
    fn target_version(&self) -> u32 {
        1
    }

    fn name(&self) -> &'static str {
        "contract_block_contexts"
    }

    fn run(&self, db: &RocksDB<BlockReplayColumnFamily>) -> anyhow::Result<()> {
        let Some(latest) = db.get_cf(
            BlockReplayColumnFamily::Latest,
            BlockReplayStorage::LATEST_KEY,
        )?
        else {
            // Fresh database: rows only ever get written in the contracted layout.
            return Ok(());
        };
        let latest = u64::from_be_bytes(
            latest
                .as_slice()
                .try_into()
                .context("malformed latest-block pointer")?,
        );

        let mut batch = db.new_write_batch();
        let mut blocks_in_batch = 0usize;
        let mut deleted = 0u64;
        for number in 0..=latest {
            let key = number.to_be_bytes();
            let legacy = db.get_cf(BlockReplayColumnFamily::Context, &key)?;

            if db
                .get_cf(BlockReplayColumnFamily::CanonicalHash, &key)?
                .is_none()
            {
                // The row predates the `CanonicalHash` CF. Its hash is embedded as the last
                // element of the successor's legacy window; the successor is only deleted when
                // its own (later) iteration commits, so it is still readable here.
                let successor = if number < latest {
                    db.get_cf(
                        BlockReplayColumnFamily::Context,
                        &(number + 1).to_be_bytes(),
                    )?
                } else {
                    None
                };
                let successor = successor.with_context(|| {
                    format!(
                        "cannot recover the canonical hash of block {number}: no CanonicalHash \
                         entry and no legacy successor row; append at least one block on a \
                         pre-contraction node version before migrating"
                    )
                })?;
                let successor_context: BlockContext =
                    bincode::serde::decode_from_slice(&successor, bincode::config::standard())
                        .context("failed to deserialize legacy successor context")?
                        .0;
                batch.put_cf(
                    BlockReplayColumnFamily::CanonicalHash,
                    &key,
                    &successor_context.block_hashes.0[255].to_be_bytes::<32>(),
                );
            }

            if let Some(legacy_bytes) = legacy.as_deref() {
                // The legacy row is authoritative: rebuild the stripped row from it even when
                // one already exists. A rebuild executed under a pre-stripped-format binary
                // (possible during a rollback) updates only the legacy row, leaving a stale
                // stripped copy behind; this heals it.
                let context: BlockContext =
                    bincode::serde::decode_from_slice(legacy_bytes, bincode::config::standard())
                        .context("failed to deserialize legacy context")?
                        .0;
                let stripped = bincode::encode_to_vec(
                    StoredBlockContextV2::strip(context),
                    bincode::config::standard(),
                )
                .context("failed to serialize stripped context")?;
                batch.put_cf(BlockReplayColumnFamily::ContextV2, &key, &stripped);
                batch.delete_cf(BlockReplayColumnFamily::Context, &key);
                deleted += 1;
            } else {
                anyhow::ensure!(
                    db.get_cf(BlockReplayColumnFamily::ContextV2, &key)?
                        .is_some(),
                    "block {number} has neither a stripped nor a legacy context row"
                );
            }

            blocks_in_batch += 1;
            if blocks_in_batch >= 1_000 {
                let full_batch = std::mem::replace(&mut batch, db.new_write_batch());
                db.write(full_batch)
                    .context("failed to commit migration chunk")?;
                blocks_in_batch = 0;
            }
            if number.is_multiple_of(1_000_000) {
                tracing::info!(number, latest, "contracting replay WAL block contexts");
            }
        }
        db.write(batch)
            .context("failed to commit final migration chunk")?;

        tracing::info!(deleted, "reclaiming legacy context space via compaction");
        db.compact_range_cf(BlockReplayColumnFamily::Context);
        Ok(())
    }
}

/// [`BlockContext`] as persisted in the `ArchivedContext` CF: the stripped fields plus this row's
/// own parent hash, in place of the 256-hash window. See
/// [`BlockReplayStorage::resolve_window_and_previous_timestamp`] for how the window and
/// `previous_block_timestamp` get reconstructed from a chain of these.
#[derive(Debug, bincode::Encode, bincode::Decode)]
struct StoredArchivedContext {
    #[bincode(with_serde)]
    parent_hash: BlockHash,
    stripped: StoredBlockContextV2,
}

/// [`BlockContext`] as persisted in the `ContextV2` CF: without the 256 previous block hashes
/// (derivable from the `CanonicalHash` CF, ~8 KiB per block; see
/// [`BlockReplayStorage::read_block_hashes`]) and without the chain id (shared by all blocks,
/// known from configuration / L1, not persisted at all — supplied by [`BlockReplayStorage`]'s
/// in-memory `chain_id` field on read).
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

    /// Rewrites canonical rows into the layout binaries wrote before the stripped format
    /// existed: a full legacy `Context` row and no `ContextV2` row.
    fn make_rows_legacy_only(storage: &BlockReplayStorage, chain: &[Sealed<ReplayRecord>]) {
        let mut batch = storage.db.new_write_batch();
        for sealed in chain {
            let key = sealed.block_context.block_number.to_be_bytes();
            let context_value =
                bincode::serde::encode_to_vec(sealed.block_context, bincode::config::standard())
                    .unwrap();
            batch.put_cf(BlockReplayColumnFamily::Context, &key, &context_value);
            batch.delete_cf(BlockReplayColumnFamily::ContextV2, &key);
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
    async fn rows_without_stripped_copy_are_served_from_legacy() {
        // Rows written by binaries predating the stripped format (reachable only by rolling
        // back further than supported) stay readable through the legacy fallback.
        let dir = tempfile::tempdir().unwrap();
        let storage = BlockReplayStorage::new_without_genesis(dir.path(), 270);
        let chain = make_chain(300);
        for sealed in &chain {
            storage.write(sealed.clone(), false).await.unwrap();
        }
        make_rows_legacy_only(&storage, &chain[100..=200]);
        assert_chain_reads_back(&storage, &chain);
    }

    #[tokio::test]
    async fn contract_migration_converts_legacy_rows() {
        let dir = tempfile::tempdir().unwrap();
        let storage = BlockReplayStorage::new_without_genesis(dir.path(), 270);
        let chain = make_chain(300);
        for sealed in &chain {
            storage.write(sealed.clone(), false).await.unwrap();
        }
        // Blocks 0..200 simulate a pre-contraction database; blocks 0..=100 additionally
        // predate the `CanonicalHash` CF, so their hashes must be recovered from successors.
        make_rows_legacy_only(&storage, &chain[..200]);
        delete_canonical_hash_entries(&storage, 0..=100);
        // Block 50 also carries a stale stripped row (a rebuild under an old binary updates
        // only the legacy row); the migration must rebuild it from the legacy one.
        let mut stale = chain[50].block_context;
        stale.timestamp += 999;
        let stale_value = bincode::encode_to_vec(
            StoredBlockContextV2::strip(stale),
            bincode::config::standard(),
        )
        .unwrap();
        let mut batch = storage.db.new_write_batch();
        batch.put_cf(
            BlockReplayColumnFamily::ContextV2,
            &50u64.to_be_bytes(),
            &stale_value,
        );
        storage.db.write(batch).unwrap();

        ContractBlockContexts.run(&storage.db).unwrap();
        // Idempotency: rerunning (e.g. after a crash before the version stamp) is a no-op.
        ContractBlockContexts.run(&storage.db).unwrap();

        assert_chain_reads_back(&storage, &chain);
        for sealed in &chain {
            let key = sealed.block_context.block_number.to_be_bytes();
            assert!(
                storage
                    .db
                    .get_cf(BlockReplayColumnFamily::Context, &key)
                    .unwrap()
                    .is_none(),
                "legacy row must be deleted"
            );
            assert!(storage.get_stored_context_v2(&key).is_some());
        }
    }

    #[tokio::test]
    async fn contract_migration_leaves_archived_rows_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let storage = BlockReplayStorage::new_without_genesis(dir.path(), 270);
        let chain = make_chain(5);
        for sealed in &chain {
            storage.write(sealed.clone(), false).await.unwrap();
        }
        let old = &chain[4];
        let mut replacement = old.as_ref().clone();
        replacement.block_context.timestamp += 100;
        storage
            .write(
                Sealed::new_unchecked(replacement.clone(), fake_hash(999)),
                true,
            )
            .await
            .unwrap();

        make_rows_legacy_only(&storage, &chain[..4]);
        ContractBlockContexts.run(&storage.db).unwrap();

        // The hash-keyed archived copy is untouched by the migration (it only visits number
        // keys) and reads back correctly.
        assert_eq!(
            storage.get_replay_record_by_key(4, Some(old.hash().0.to_vec())),
            Some(old.as_ref().clone())
        );
        assert_eq!(storage.get_replay_record(4), Some(replacement));
    }

    #[tokio::test]
    async fn contract_migration_needs_recoverable_tip_hash() {
        let dir = tempfile::tempdir().unwrap();
        let storage = BlockReplayStorage::new_without_genesis(dir.path(), 270);
        let chain = make_chain(50);
        for sealed in &chain {
            storage.write(sealed.clone(), false).await.unwrap();
        }
        // A chain whose every row predates `CanonicalHash`: all hashes except the tip's are
        // recoverable from successors.
        make_rows_legacy_only(&storage, &chain);
        delete_canonical_hash_entries(&storage, 0..=49);

        let err = ContractBlockContexts.run(&storage.db).unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot recover the canonical hash of block 49"),
            "unexpected error: {err:#}"
        );
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

        // The replaced record stays readable under its hash key, window and
        // `previous_block_timestamp` both reconstructed correctly.
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
        // must not be reconstructed from the index — nor must its `previous_block_timestamp`,
        // which would otherwise pick up new block 3's timestamp instead of old block 3's.
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

        // The archived copies are byte-correct: old block 4's window ends with old block 3's
        // hash (not the replacement's), and its `previous_block_timestamp` is old block 3's
        // timestamp (not new block 3's) — both resolved by walking `parent_hash` pointers
        // through `ArchivedContext`, not by any in-memory wave state.
        assert_eq!(
            storage.get_replay_record_by_key(4, Some(old4.hash().0.to_vec())),
            Some(old4.as_ref().clone())
        );
        assert_eq!(
            storage.get_replay_record_by_key(3, Some(old3.hash().0.to_vec())),
            Some(old3.as_ref().clone())
        );
        // The new canonical rows reconstruct against the repointed index.
        assert_eq!(storage.get_replay_record(3), Some(new3));
        assert_eq!(storage.get_replay_record(4), Some(new4));
    }

    #[tokio::test]
    async fn archived_rows_read_correctly_after_wave_state_is_gone() {
        // The whole point of resolving archived rows by walking `parent_hash` pointers instead
        // of an in-memory wave cache: they must read back correctly even after that cache is
        // long gone — e.g. a fresh process, reading via `en_replay_record_overrides` well after
        // the override happened.
        let dir = tempfile::tempdir().unwrap();
        let storage = BlockReplayStorage::new_without_genesis(dir.path(), 270);
        let chain = make_chain(5);
        for sealed in &chain {
            storage.write(sealed.clone(), false).await.unwrap();
        }

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

        // A plain append ends the wave, clearing any in-memory state.
        let mut new5 = new4.clone();
        new5.block_context.block_number = 5;
        new5.block_context.block_hashes = new4.block_context.block_hashes.push(fake_hash(104));
        new5.previous_block_timestamp = new4.block_context.timestamp;
        storage
            .write(Sealed::new_unchecked(new5, fake_hash(5)), false)
            .await
            .unwrap();

        // Reopening drops any remaining in-memory state outright.
        drop(storage);
        let storage = BlockReplayStorage::new_without_genesis(dir.path(), 270);

        assert_eq!(
            storage.get_replay_record_by_key(4, Some(old4.hash().0.to_vec())),
            Some(old4.as_ref().clone())
        );
        assert_eq!(
            storage.get_replay_record_by_key(3, Some(old3.hash().0.to_vec())),
            Some(old3.as_ref().clone())
        );
    }
}
