//! Snapshot trimmer for zksync-os-server databases.
//!
//! Reduces an 8GB+ production snapshot to a small bootstrap artifact by
//! removing historical data that the server doesn't need on startup.
//!
//! What gets trimmed:
//! - block_replay_wal: keeps last `--keep-blocks` blocks (default 2000), deletes the rest
//! - repository:       keeps last `--keep-blocks` blocks (block/tx/receipt data)
//!
//! `--keep-blocks` must exceed the L1 execution lag (WAL tip - last_l1_executed_block) at
//! snapshot time, or the server panics on startup ("replay record must exist"). That lag is
//! not stored in the snapshot, so the default keeps a generous margin; see the --keep-blocks
//! and --allow-small-keep flags.
//! - state_full_diffs: keeps the most-recent write per storage key (drops old
//!                     multi-version history, preserving correctness at the cutoff block)
//!
//! What is left untouched:
//! - preimages_full_diffs: content-addressed, nothing to remove
//! - batch:                small enough to leave as-is
//!
//! Snapshot normalization (on by default; --no-normalize to skip):
//! - A DB copied from a *running* node can have state_full_diffs a block or two ahead of the WAL
//!   (the sub-DBs are persisted asynchronously). The trimmer truncates state back down to the WAL
//!   tip so the snapshot is internally consistent; otherwise the server panics on startup with a
//!   "historical write discrepancy".
//!
//! Tree GC (on by default; --no-trim-tree to skip):
//! - zkos_merkle_tree: GC from the kept roots, keeping only reachable nodes.
//!   Typically shrinks the tree from ~1 GB to ~10-30 MB.

use alloy_rlp::{Decodable as _, Header as RlpHeader};
use anyhow::{Context, bail};
use clap::Parser;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::info;
use zksync_os_merkle_tree::MerkleTreeColumnFamily;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::NamedColumnFamily;

// ── Column family definitions ─────────────────────────────────────────────────
// These mirror the production types exactly (same DB_NAME and CF name strings).

#[derive(Copy, Clone, Debug)]
enum BlockReplayCF {
    Context,
    StartingL1SerialId,
    Txs,
    NodeVersion,
    ProtocolVersion,
    BlockOutputHash,
    ForcePreimages,
    StartingInteropRootId,
    StartingMigrationNumber,
    StartingInteropFeeNumber,
    CanonicalHash,
    Latest,
}

impl NamedColumnFamily for BlockReplayCF {
    const DB_NAME: &'static str = "block_replay_wal";
    const ALL: &'static [Self] = &[
        Self::Context,
        Self::StartingL1SerialId,
        Self::Txs,
        Self::NodeVersion,
        Self::ProtocolVersion,
        Self::BlockOutputHash,
        Self::ForcePreimages,
        Self::StartingInteropRootId,
        Self::StartingMigrationNumber,
        Self::StartingInteropFeeNumber,
        Self::CanonicalHash,
        Self::Latest,
    ];

    fn name(&self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::StartingL1SerialId => "last_processed_l1_tx_id",
            Self::Txs => "txs",
            Self::NodeVersion => "node_version",
            Self::ProtocolVersion => "protocol_version",
            Self::BlockOutputHash => "block_output_hash",
            Self::ForcePreimages => "force_preimages",
            Self::StartingInteropRootId => "starting_interop_root_id",
            Self::StartingMigrationNumber => "starting_migration_number",
            Self::StartingInteropFeeNumber => "starting_interop_fee_number",
            Self::CanonicalHash => "canonical_hash",
            Self::Latest => "latest",
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum RepositoryCF {
    BlockData,
    BlockNumberToHash,
    Tx,
    TxReceipt,
    TxMeta,
    InitiatorAndNonceToHash,
    Meta,
    LogBlocksByAddress,
    LogBlocksByTopic,
}

impl NamedColumnFamily for RepositoryCF {
    const DB_NAME: &'static str = "repository";
    const ALL: &'static [Self] = &[
        Self::BlockData,
        Self::BlockNumberToHash,
        Self::Tx,
        Self::TxReceipt,
        Self::TxMeta,
        Self::InitiatorAndNonceToHash,
        Self::Meta,
        Self::LogBlocksByAddress,
        Self::LogBlocksByTopic,
    ];

    fn name(&self) -> &'static str {
        match self {
            Self::BlockData => "block_data",
            Self::BlockNumberToHash => "block_number_to_hash",
            Self::Tx => "tx",
            Self::TxReceipt => "tx_receipt",
            Self::TxMeta => "tx_meta",
            Self::InitiatorAndNonceToHash => "initiator_and_nonce_to_hash",
            Self::Meta => "meta",
            Self::LogBlocksByAddress => "log_blocks_by_address",
            Self::LogBlocksByTopic => "log_blocks_by_topic",
        }
    }

    fn prefix_extractor_len(&self) -> Option<usize> {
        match self {
            Self::LogBlocksByAddress => Some(20),
            Self::LogBlocksByTopic => Some(32),
            _ => None,
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum StateDiffsCF {
    Data,
    Meta,
}

impl NamedColumnFamily for StateDiffsCF {
    const DB_NAME: &'static str = "state_full_diffs";
    const ALL: &'static [Self] = &[Self::Data, Self::Meta];

    fn name(&self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Meta => "meta",
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum BatchCF {
    BatchInfo,
    FirstBlockIndex,
    Latest,
}

impl NamedColumnFamily for BatchCF {
    const DB_NAME: &'static str = "executed_batch_storage";
    const ALL: &'static [Self] = &[Self::BatchInfo, Self::FirstBlockIndex, Self::Latest];

    fn name(&self) -> &'static str {
        match self {
            Self::BatchInfo => "batch_info",
            Self::FirstBlockIndex => "first_block_index",
            Self::Latest => "latest",
        }
    }
}

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "snapshot-trimmer",
    about = "Trim a zksync-os-server DB snapshot to a small bootstrap artifact",
    long_about = "Removes historical data older than --keep-blocks blocks from block_replay_wal,\n\
                  repository, and state_full_diffs, and GCs the merkle tree. By default it also\n\
                  force-compacts to reclaim disk space immediately. Preimages and batch DBs are\n\
                  left untouched.\n\
                  \n\
                  --db-dir may point either at the node DB directory itself or at a snapshot\n\
                  parent directory containing `node1/` (e.g. a freshly `kubectl cp`'d folder).\n\
                  In the latter case, non-node siblings like `fri_proofs/` are deleted by default.\n\
                  See --no-trim-tree, --no-compact, and --no-cleanup to opt out."
)]
struct Cli {
    /// Path to the node DB directory (contains block_replay_wal/, repository/, etc.), OR a
    /// snapshot parent directory containing a `node1/` subdirectory (e.g. a `kubectl cp`'d
    /// folder that also holds fri_proofs/, block_dumps/, …). In the parent-dir case those
    /// non-node siblings are cleaned up by default — see --no-cleanup.
    #[arg(long)]
    db_dir: PathBuf,

    /// How many recent blocks to keep in the WAL and repository.
    /// Blocks older than (latest - keep_blocks) are deleted.
    ///
    /// This must be large enough to cover the gap between the WAL tip and
    /// `last_l1_executed_block` at snapshot time: on startup the server replays from the
    /// oldest block it still needs, which is dominated by how far L1 *execution* lags behind
    /// the sequencer. That number lives on L1 (it is re-fetched every startup and is never
    /// stored in the snapshot), so the trimmer cannot compute the exact minimum — it keeps a
    /// generous fixed margin instead. Too small a value silently produces a snapshot that
    /// panics with "replay record must exist". See --allow-small-keep.
    #[arg(long, default_value = "2000")]
    keep_blocks: u64,

    /// Allow --keep-blocks below the recommended safe floor.
    ///
    /// By default the trimmer refuses small values because they risk cutting below
    /// `last_l1_executed_block` (the L1 execution lag), producing a snapshot that looks fine
    /// but panics on server startup. Pass this only if you know the L1 execution lag for this
    /// snapshot is smaller than --keep-blocks.
    #[arg(long, default_value = "false")]
    allow_small_keep: bool,

    /// Dry run: print what would be trimmed without writing any changes.
    #[arg(long, default_value = "false")]
    dry_run: bool,

    /// Skip force-compaction. By default each trimmed column family is compacted immediately to
    /// reclaim disk space; with this flag RocksDB compacts lazily in the background when the
    /// server next runs (the snapshot stays large on disk until then).
    #[arg(long, default_value = "false")]
    no_compact: bool,

    /// Skip merkle-tree GC. By default the tree is GC'd (traverse from the kept roots, delete all
    /// unreachable historical nodes), typically shrinking it from ~1 GB to ~10-30 MB. Use this to
    /// leave the tree untouched.
    #[arg(long, default_value = "false")]
    no_trim_tree: bool,

    /// Skip cleanup of non-node files. When --db-dir is a snapshot parent directory (i.e. it
    /// contains a `node1/` subdirectory), the trimmer by default deletes every sibling of
    /// `node1/` — generated data like `fri_proofs/` and `block_dumps/` that the server does not
    /// need on startup. Use this to leave those in place. No effect when --db-dir already points
    /// directly at the node DB directory.
    #[arg(long, default_value = "false")]
    no_cleanup: bool,

    /// Read-only: print which batch covers a block and how to pick the matching L1 fork block, then
    /// exit without trimming. Pass a block number, or pass the flag alone to use the snapshot's tip
    /// (the min height across sub-DBs — where the normalized snapshot ends up). Use when booting a
    /// main node from a snapshot against a forked L1 — L1's committed tip must be <= the snapshot
    /// tip, or the node stalls. Reads only the snapshot's batch DB; no L1 connection required (the
    /// L1 hop is printed as a `cast` recipe).
    #[arg(long, num_args = 0..=1)]
    print_batch_for_block: Option<Option<u64>>,

    /// Skip state normalization. By default, if `state_full_diffs` is ahead of the WAL tip (which
    /// happens when the DB is copied from a *running* node), the trimmer truncates state back down
    /// to the WAL tip so the snapshot is internally consistent. Without this, the server panics on
    /// startup with a "historical write discrepancy". Use this only if you know the snapshot was
    /// taken from a cleanly stopped node.
    #[arg(long, default_value = "false")]
    no_normalize: bool,
}

impl Cli {
    fn compact(&self) -> bool {
        !self.no_compact
    }
    fn trim_tree(&self) -> bool {
        !self.no_trim_tree
    }
    fn cleanup(&self) -> bool {
        !self.no_cleanup
    }
    fn normalize(&self) -> bool {
        !self.no_normalize
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn be_block_key(block_number: u64) -> [u8; 8] {
    block_number.to_be_bytes()
}

// ── WAL trimmer ───────────────────────────────────────────────────────────────

/// Trims the WAL and returns its tip (latest block number). The tip is the replay source of
/// truth — every other sub-DB must be aligned to not exceed it (see `normalize_state_to_tip`).
fn trim_wal(db_path: &Path, keep_blocks: u64, dry_run: bool, compact: bool) -> anyhow::Result<u64> {
    info!("Opening block_replay_wal at {:?}", db_path);
    let db = RocksDB::<BlockReplayCF>::new(db_path)
        .with_context(|| format!("failed to open block_replay_wal at {db_path:?}"))?;

    // Read latest block from the Latest CF.
    let latest_bytes = db
        .get_cf(BlockReplayCF::Latest, b"latest_block")
        .context("failed to read Latest CF")?
        .context("block_replay_wal Latest key missing — is DB empty?")?;
    let latest: u64 = u64::from_be_bytes(
        latest_bytes
            .as_slice()
            .try_into()
            .context("Latest key has unexpected length")?,
    );

    if latest < keep_blocks {
        info!(
            "WAL latest block {latest} < keep_blocks {keep_blocks}, nothing to trim"
        );
        return Ok(latest);
    }
    let cutoff = latest - keep_blocks;
    info!("WAL: latest={latest}, keep_blocks={keep_blocks}, deleting blocks 0..{cutoff}");

    if dry_run {
        info!("[dry-run] would delete WAL entries for blocks 0..{cutoff} across all CFs");
        return Ok(latest);
    }

    // delete_range is exclusive on end, so range [start_key, end_key) covers 0..cutoff.
    let start = be_block_key(0);
    let end = be_block_key(cutoff);

    // All data CFs use block_number_be as key. Delete the range for each.
    let data_cfs = [
        BlockReplayCF::Context,
        BlockReplayCF::StartingL1SerialId,
        BlockReplayCF::Txs,
        BlockReplayCF::NodeVersion,
        BlockReplayCF::ProtocolVersion,
        BlockReplayCF::BlockOutputHash,
        BlockReplayCF::ForcePreimages,
        BlockReplayCF::StartingInteropRootId,
        BlockReplayCF::StartingMigrationNumber,
        BlockReplayCF::StartingInteropFeeNumber,
        BlockReplayCF::CanonicalHash,
    ];

    let mut batch = db.new_write_batch();
    for cf in data_cfs {
        batch.delete_range_cf(cf, &start..&end);
    }
    db.write(batch).context("failed to write WAL trim batch")?;
    info!("WAL trimmed: deleted blocks 0..{cutoff}, kept {keep_blocks}+1 blocks");

    if compact {
        info!("Compacting WAL column families...");
        for cf in data_cfs {
            db.compact_cf(cf);
        }
        info!("WAL compaction done");
    }
    Ok(latest)
}

// ── Snapshot normalization (cross-DB consistency) ──────────────────────────────
//
// A DB copied from a *running* node can be internally inconsistent: the sub-DBs are independent
// RocksDB instances persisted asynchronously, so at copy time they can sit at different block
// heights. Worse, a node that crashed/was killed mid-block can leave the WAL and state holding a
// half-produced block that the downstream tree/repository never received — and the WAL's copy of
// that block can even disagree with state's copy (different executions of the same block number).
//
// On restart the server replays the WAL up to its tip and validates each re-applied block against
// the stored state (`FullDiffsStorage::add_block`, `state_full_diffs/src/storage.rs:93-103`). Any
// disagreement panics with a "historical write discrepancy".
//
// Fix: align every sub-DB to the highest block at which they are all known-consistent — the
// minimum latest-block across WAL, state, tree, and repository. Blocks above that floor in the WAL
// and state are uncommitted in-flight work; we delete them so the server re-produces them cleanly.
// (Tree/repository sit at the trailing edge of the pipeline, so in practice they define the floor
// and need no truncation; if one is somehow ahead of the floor we bail rather than ship a broken
// snapshot, since rolling those back is not yet supported here.)

const STATE_LATEST_BLOCK_KEY: &[u8] = b"latest_block";
const WAL_LATEST_BLOCK_KEY: &[u8] = b"latest_block";
const REPO_BLOCK_NUMBER_KEY: &[u8] = b"block_number";

fn read_u64_be(bytes: &[u8]) -> anyhow::Result<u64> {
    Ok(u64::from_be_bytes(
        bytes.try_into().context("expected 8-byte big-endian u64")?,
    ))
}

fn read_wal_latest(db_path: &Path) -> anyhow::Result<u64> {
    let db = RocksDB::<BlockReplayCF>::new(db_path)
        .with_context(|| format!("failed to open block_replay_wal at {db_path:?}"))?;
    let v = db
        .get_cf(BlockReplayCF::Latest, WAL_LATEST_BLOCK_KEY)
        .context("failed to read WAL Latest")?
        .context("block_replay_wal Latest key missing")?;
    read_u64_be(&v)
}

fn read_state_latest(db_path: &Path) -> anyhow::Result<u64> {
    let db = RocksDB::<StateDiffsCF>::new(db_path)
        .with_context(|| format!("failed to open state_full_diffs at {db_path:?}"))?;
    match db
        .get_cf(StateDiffsCF::Meta, STATE_LATEST_BLOCK_KEY)
        .context("failed to read state_full_diffs Meta")?
    {
        Some(v) => read_u64_be(&v),
        None => Ok(0),
    }
}

fn read_repo_latest(db_path: &Path) -> anyhow::Result<u64> {
    let db = RocksDB::<RepositoryCF>::new(db_path)
        .with_context(|| format!("failed to open repository at {db_path:?}"))?;
    match db
        .get_cf(RepositoryCF::Meta, REPO_BLOCK_NUMBER_KEY)
        .context("failed to read repository Meta")?
    {
        Some(v) => read_u64_be(&v),
        None => Ok(0),
    }
}

/// Tree version N corresponds to block N (genesis = version 0 = block 0), so the latest tree block
/// is `version_count - 1`. Returns None if the tree DB / manifest is absent.
fn read_tree_latest(db_path: &Path) -> anyhow::Result<Option<u64>> {
    if !db_path.join("CURRENT").exists() {
        return Ok(None);
    }
    let db = RocksDB::<MerkleTreeColumnFamily>::new(db_path)
        .with_context(|| format!("failed to open zkos_merkle_tree at {db_path:?}"))?;
    match db
        .get_cf(MerkleTreeColumnFamily::Tree, &[0])
        .context("failed to read tree manifest")?
    {
        Some(manifest) => {
            let (version_count, _, _) = parse_tree_manifest(&manifest)?;
            Ok(version_count.checked_sub(1))
        }
        None => Ok(None),
    }
}

/// Deletes WAL entries for blocks above `anchor` and resets the Latest pointer.
fn truncate_wal_to(db_path: &Path, anchor: u64, dry_run: bool) -> anyhow::Result<()> {
    let db = RocksDB::<BlockReplayCF>::new(db_path)
        .with_context(|| format!("failed to open block_replay_wal at {db_path:?}"))?;
    let latest = read_u64_be(
        &db.get_cf(BlockReplayCF::Latest, WAL_LATEST_BLOCK_KEY)
            .context("failed to read WAL Latest")?
            .context("block_replay_wal Latest key missing")?,
    )?;
    if latest <= anchor {
        return Ok(());
    }
    info!("Normalizing WAL: latest={latest} ahead of anchor {anchor} — truncating to {anchor}");
    if dry_run {
        info!("[dry-run] would delete WAL blocks {}..={latest} and set latest_block={anchor}", anchor + 1);
        return Ok(());
    }
    let start = be_block_key(anchor + 1);
    let end = be_block_key(latest + 1); // exclusive end → covers (anchor, latest]
    let data_cfs = [
        BlockReplayCF::Context,
        BlockReplayCF::StartingL1SerialId,
        BlockReplayCF::Txs,
        BlockReplayCF::NodeVersion,
        BlockReplayCF::ProtocolVersion,
        BlockReplayCF::BlockOutputHash,
        BlockReplayCF::ForcePreimages,
        BlockReplayCF::StartingInteropRootId,
        BlockReplayCF::StartingMigrationNumber,
        BlockReplayCF::StartingInteropFeeNumber,
        BlockReplayCF::CanonicalHash,
    ];
    let mut batch = db.new_write_batch();
    for cf in data_cfs {
        batch.delete_range_cf(cf, &start..&end);
    }
    batch.put_cf(BlockReplayCF::Latest, WAL_LATEST_BLOCK_KEY, anchor.to_be_bytes().as_ref());
    db.write(batch).context("failed to write WAL truncation batch")?;
    info!("WAL normalized to block {anchor}");
    Ok(())
}

/// Deletes state entries for blocks above `anchor` and resets the latest_block pointer.
fn truncate_state_to(db_path: &Path, anchor: u64, dry_run: bool) -> anyhow::Result<()> {
    let db = RocksDB::<StateDiffsCF>::new(db_path)
        .with_context(|| format!("failed to open state_full_diffs at {db_path:?}"))?;
    // Read latest from the already-open handle (re-opening would deadlock on the RocksDB LOCK).
    let state_latest = match db
        .get_cf(StateDiffsCF::Meta, STATE_LATEST_BLOCK_KEY)
        .context("failed to read state_full_diffs Meta")?
    {
        Some(v) => read_u64_be(&v)?,
        None => 0,
    };
    if state_latest <= anchor {
        return Ok(());
    }
    info!(
        "Normalizing state: latest={state_latest} ahead of anchor {anchor} — truncating to {anchor}"
    );
    if dry_run {
        info!("[dry-run] would delete state blocks {}..={state_latest} and set latest_block={anchor}", anchor + 1);
        return Ok(());
    }
    // Data keys: hashed_key[32] || block_number_be[8]. Delete every entry above the anchor.
    let mut to_delete: Vec<Vec<u8>> = Vec::new();
    for (k, _v) in db.prefix_iterator_cf(StateDiffsCF::Data, &[]) {
        if k.len() != 40 {
            bail!("unexpected key length {} in state_full_diffs Data CF (expected 40)", k.len());
        }
        let block_num = u64::from_be_bytes(k[32..40].try_into().unwrap());
        if block_num > anchor {
            to_delete.push(k.to_vec());
        }
    }
    info!("Normalizing state: deleting {} entries above block {anchor}", to_delete.len());
    for chunk in to_delete.chunks(100_000) {
        let mut batch = db.new_write_batch();
        for key in chunk {
            batch.delete_cf(StateDiffsCF::Data, key);
        }
        db.write(batch).context("failed to write state truncation batch")?;
    }
    let mut batch = db.new_write_batch();
    batch.put_cf(StateDiffsCF::Meta, STATE_LATEST_BLOCK_KEY, anchor.to_be_bytes().as_ref());
    db.write(batch).context("failed to reset state latest_block")?;
    info!("State normalized to block {anchor}");
    Ok(())
}

/// Deletes repository blocks above `anchor` and resets the block_number pointer. Mirrors the
/// deletion in `RepositoryManager::rollback` (`storage/src/db/repository.rs:231`), but finds the
/// per-tx records via a TxMeta scan instead of decoding block bodies. Leaves the log-index bitmaps
/// and InitiatorAndNonceToHash entries as-is: the truncated blocks get re-produced deterministically
/// on startup, and re-indexing sets the same bits (idempotent), so they self-heal.
fn truncate_repo_to(db_path: &Path, anchor: u64, dry_run: bool) -> anyhow::Result<()> {
    let db = RocksDB::<RepositoryCF>::new(db_path)
        .with_context(|| format!("failed to open repository at {db_path:?}"))?;
    let repo_latest = match db
        .get_cf(RepositoryCF::Meta, REPO_BLOCK_NUMBER_KEY)
        .context("failed to read repository Meta")?
    {
        Some(v) => read_u64_be(&v)?,
        None => return Ok(()),
    };
    if repo_latest <= anchor {
        return Ok(());
    }
    info!(
        "Normalizing repository: latest={repo_latest} ahead of anchor {anchor} — truncating to {anchor}"
    );
    if dry_run {
        info!("[dry-run] would delete repository blocks {}..={repo_latest} and set block_number={anchor}", anchor + 1);
        return Ok(());
    }

    // Delete block-keyed records (BlockData via hash, BlockNumberToHash) for (anchor, repo_latest].
    let mut batch = db.new_write_batch();
    for block in (anchor + 1)..=repo_latest {
        let block_key = be_block_key(block);
        if let Some(hash) = db
            .get_cf(RepositoryCF::BlockNumberToHash, &block_key)
            .with_context(|| format!("BlockNumberToHash read failed at block {block}"))?
        {
            batch.delete_cf(RepositoryCF::BlockData, &hash);
            batch.delete_cf(RepositoryCF::BlockNumberToHash, &block_key);
        }
    }
    batch.put_cf(RepositoryCF::Meta, REPO_BLOCK_NUMBER_KEY, anchor.to_be_bytes().as_ref());
    db.write(batch).context("failed to write repository truncation batch")?;

    // Delete tx-keyed records (Tx/TxReceipt/TxMeta) for the truncated blocks via a TxMeta scan.
    let mut tx_hashes: Vec<Vec<u8>> = Vec::new();
    for (k, v) in db.prefix_iterator_cf(RepositoryCF::TxMeta, &[]) {
        if tx_meta_block_number(&v)? > anchor {
            tx_hashes.push(k.to_vec());
        }
    }
    for chunk in tx_hashes.chunks(10_000) {
        let mut batch = db.new_write_batch();
        for tx_hash in chunk {
            batch.delete_cf(RepositoryCF::Tx, tx_hash);
            batch.delete_cf(RepositoryCF::TxReceipt, tx_hash);
            batch.delete_cf(RepositoryCF::TxMeta, tx_hash);
        }
        db.write(batch).context("failed to write repository tx truncation batch")?;
    }
    info!("Repository normalized to block {anchor} (deleted {} tx records)", tx_hashes.len());
    Ok(())
}

/// Parses the tree manifest fully: returns (version_count, all tags) so it can be re-serialized.
fn parse_tree_manifest_full(bytes: &[u8]) -> anyhow::Result<(u64, Vec<(String, String)>)> {
    let buf = &mut &bytes[..];
    let version_count = leb128_read_u64(buf)?;
    let tag_count = leb128_read_u64(buf)?;
    let mut tags = Vec::with_capacity(tag_count as usize);
    for _ in 0..tag_count {
        let key = leb128_read_str(buf)?.to_string();
        let value = leb128_read_str(buf)?.to_string();
        tags.push((key, value));
    }
    Ok((version_count, tags))
}

fn serialize_tree_manifest(version_count: u64, tags: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    leb128::write::unsigned(&mut out, version_count).unwrap();
    leb128::write::unsigned(&mut out, tags.len() as u64).unwrap();
    for (k, v) in tags {
        leb128::write::unsigned(&mut out, k.len() as u64).unwrap();
        out.extend_from_slice(k.as_bytes());
        leb128::write::unsigned(&mut out, v.len() as u64).unwrap();
        out.extend_from_slice(v.as_bytes());
    }
    out
}

/// Lowers the tree manifest's version_count to `anchor + 1` if it is higher. The now-orphaned nodes
/// for versions above `anchor` are unreachable from root(anchor) and get removed by the subsequent
/// tree GC (or remain as harmless dead space if --no-trim-tree). root(anchor) and all nodes it
/// references (version <= anchor) are untouched.
fn truncate_tree_to(db_path: &Path, anchor: u64, dry_run: bool) -> anyhow::Result<()> {
    if !db_path.join("CURRENT").exists() {
        return Ok(());
    }
    let db = RocksDB::<MerkleTreeColumnFamily>::new(db_path)
        .with_context(|| format!("failed to open zkos_merkle_tree at {db_path:?}"))?;
    let Some(manifest) = db
        .get_cf(MerkleTreeColumnFamily::Tree, &[0])
        .context("failed to read tree manifest")?
    else {
        return Ok(());
    };
    let (version_count, tags) = parse_tree_manifest_full(&manifest)?;
    let new_version_count = anchor + 1;
    if version_count <= new_version_count {
        return Ok(());
    }
    info!(
        "Normalizing tree: version_count={version_count} (latest={}) ahead of anchor {anchor} — \
         lowering version_count to {new_version_count}",
        version_count - 1
    );
    if dry_run {
        info!("[dry-run] would set tree manifest version_count={new_version_count}");
        return Ok(());
    }
    let new_manifest = serialize_tree_manifest(new_version_count, &tags);
    let mut batch = db.new_write_batch();
    batch.put_cf(MerkleTreeColumnFamily::Tree, &[0], &new_manifest);
    db.write(batch).context("failed to write tree manifest")?;
    info!("Tree manifest lowered to version_count={new_version_count}; orphan versions left for GC");
    Ok(())
}

/// The snapshot's effective tip: the minimum latest-block across all sub-DBs. After normalization
/// the whole snapshot sits at this height, so it's the right default for batch/fork-block queries.
fn snapshot_tip(node_dir: &Path) -> anyhow::Result<u64> {
    let wal = read_wal_latest(&node_dir.join("block_replay_wal"))?;
    let state = read_state_latest(&node_dir.join("state_full_diffs"))?;
    let repo = read_repo_latest(&node_dir.join("repository"))?;
    let mut tip = wal.min(state).min(repo);
    if let Some(t) = read_tree_latest(&node_dir.join("tree"))? {
        tip = tip.min(t);
    }
    Ok(tip)
}

/// Reads a batch's inclusive [start, end] block range from its BatchInfo JSON
/// (`{"block_range":{"start":S,"end":E}}`, where `end` is inclusive).
fn batch_block_range(db: &RocksDB<BatchCF>, batch: u64) -> anyhow::Result<Option<(u64, u64)>> {
    let Some(bytes) = db
        .get_cf(BatchCF::BatchInfo, &batch.to_be_bytes())
        .context("failed to read BatchInfo")?
    else {
        return Ok(None);
    };
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).context("failed to parse BatchInfo JSON")?;
    let range = &v["block_range"];
    let start = range["start"].as_u64().context("block_range.start missing")?;
    let end = range["end"].as_u64().context("block_range.end missing")?;
    Ok(Some((start, end)))
}

/// Reads the snapshot's executed_batch_storage to report which batch covers `target_block`, and
/// prints how to pick a matching L1 fork block. Read-only; no L1 connection.
fn print_batch_for_block(node_dir: &Path, target_block: u64) -> anyhow::Result<()> {
    let batch_path = node_dir.join("batch");
    if !batch_path.join("CURRENT").exists() {
        bail!("no executed_batch_storage (batch DB) found at {batch_path:?}");
    }
    let db = RocksDB::<BatchCF>::new(&batch_path)
        .with_context(|| format!("failed to open executed_batch_storage at {batch_path:?}"))?;

    // FirstBlockIndex: first_block_number(be) -> batch_number(be). The batch whose range may cover
    // `target_block` is the one with the largest first_block <= target_block.
    let target_be = target_block.to_be_bytes();
    let Some((_, batch_val)) = db
        .to_iterator_cf(BatchCF::FirstBlockIndex, ..=target_be.as_slice())
        .next()
    else {
        bail!("no batch found at or before block {target_block} — is the block below the snapshot's range?");
    };
    let batch = read_u64_be(&batch_val[..8])?;
    let (start, end) = batch_block_range(&db, batch)?
        .with_context(|| format!("batch {batch} indexed in FirstBlockIndex but missing from BatchInfo"))?;

    // Choose the highest batch whose last block is <= target_block, so L1's committed tip — which
    // the node maps back through *this same* batch DB on startup — stays <= the snapshot tip.
    let (fork_batch, committed_tip) = if target_block >= end {
        // target is at or beyond this (latest applicable) batch's end → committing it is safe.
        if target_block > end {
            info!(
                "Block {target_block} is beyond the last locally-built batch {batch} (ends at block \
                 {end}) — the snapshot's batcher lagged block production. L1 likely has it in a later \
                 batch this snapshot doesn't know; the safe fork target is batch {batch}."
            );
        } else {
            info!("Block {target_block} is the last block of batch {batch} (blocks {start}..={end}).");
        }
        (batch, end)
    } else {
        // target is mid-batch → committing batch `batch` would push the tip past target; stop one short.
        info!("Block {target_block} is inside batch {batch} (blocks {start}..={end}).");
        (batch.saturating_sub(1), start.saturating_sub(1))
    };

    info!("");
    info!("To boot a main node from a snapshot whose tip is block {target_block}, L1's committed tip");
    info!("must be <= {target_block}. Fork L1 where getTotalBatchesCommitted() == {fork_batch}");
    info!("(committed tip = block {committed_tip}, <= {target_block}).");
    info!("");
    info!("Find that L1 block by binary-searching the diamond proxy on your fork:");
    info!("  cast call <DIAMOND_PROXY> 'getTotalBatchesCommitted()(uint256)' --block <L1_BLOCK> --rpc-url <L1_RPC>");
    info!("Pick the highest L1 block whose result is <= {fork_batch}, then:");
    info!("  anvil --fork-url <SEPOLIA_RPC> --fork-block-number <that L1 block>");
    Ok(())
}

/// Aligns every sub-DB down to the common consistent height (the minimum latest-block across
/// all sub-DBs), so a snapshot copied from a live/crashed node starts cleanly. Returns the anchor.
fn normalize_snapshot(node_dir: &Path, dry_run: bool) -> anyhow::Result<u64> {
    let wal_latest = read_wal_latest(&node_dir.join("block_replay_wal"))?;
    let state_latest = read_state_latest(&node_dir.join("state_full_diffs"))?;
    let repo_latest = read_repo_latest(&node_dir.join("repository"))?;
    // Always factor the tree height into the anchor, even with --no-trim-tree.
    let tree_latest = read_tree_latest(&node_dir.join("tree"))?;

    let mut anchor = wal_latest.min(state_latest).min(repo_latest);
    if let Some(t) = tree_latest {
        anchor = anchor.min(t);
    }

    info!(
        "Heights — wal={wal_latest}, state={state_latest}, repo={repo_latest}, tree={tree_latest:?}; \
         common consistent anchor={anchor}"
    );

    let already_consistent = wal_latest == anchor
        && state_latest == anchor
        && repo_latest == anchor
        && tree_latest.is_none_or(|t| t == anchor);
    if already_consistent {
        info!("Snapshot already consistent at block {anchor}; no normalization needed");
        return Ok(anchor);
    }

    // Roll every sub-DB that's ahead of the floor back down to it.
    truncate_wal_to(&node_dir.join("block_replay_wal"), anchor, dry_run)?;
    truncate_state_to(&node_dir.join("state_full_diffs"), anchor, dry_run)?;
    truncate_repo_to(&node_dir.join("repository"), anchor, dry_run)?;
    truncate_tree_to(&node_dir.join("tree"), anchor, dry_run)?;

    Ok(anchor)
}

// ── TxMeta decode helper ──────────────────────────────────────────────────────

// TxMeta is RLP-encoded as: LIST(B256, u64 block_number, u64 block_timestamp, ...).
// We only need block_number — decode the first two fields and stop.
fn tx_meta_block_number(bytes: &[u8]) -> anyhow::Result<u64> {
    let buf = &mut &bytes[..];
    let header = RlpHeader::decode(buf).context("decode TxMeta list header")?;
    if !header.list {
        bail!("TxMeta RLP is not a list");
    }
    // Skip B256 block_hash (always 32-byte RLP string → header + 32 bytes).
    let _ = <[u8; 32]>::decode(buf).context("decode TxMeta block_hash")?;
    // Decode block_number.
    let block_number = u64::decode(buf).context("decode TxMeta block_number")?;
    Ok(block_number)
}

// ── Repository trimmer ────────────────────────────────────────────────────────

fn trim_repository(db_path: &Path, keep_blocks: u64, dry_run: bool, compact: bool) -> anyhow::Result<()> {
    info!("Opening repository at {:?}", db_path);
    let db = RocksDB::<RepositoryCF>::new(db_path)
        .with_context(|| format!("failed to open repository at {db_path:?}"))?;

    let latest_bytes = db
        .get_cf(RepositoryCF::Meta, b"block_number")
        .context("failed to read repository Meta")?
        .context("repository Meta block_number missing — is DB empty?")?;
    let latest: u64 = u64::from_be_bytes(
        latest_bytes
            .as_slice()
            .try_into()
            .context("Meta block_number has unexpected length")?,
    );

    if latest < keep_blocks {
        info!(
            "Repository latest block {latest} < keep_blocks {keep_blocks}, nothing to trim"
        );
        return Ok(());
    }
    let cutoff = latest - keep_blocks;
    info!(
        "Repository: latest={latest}, keep_blocks={keep_blocks}, deleting blocks 0..{cutoff}"
    );

    if dry_run {
        info!(
            "[dry-run] would delete repository blocks 0..{cutoff} \
             (block data, txs, receipts, meta — log index bitmaps left as-is)"
        );
        return Ok(());
    }

    // Walk each block in [0, cutoff), collecting tx hashes so we can delete tx-keyed records.
    // We accumulate into write batches and flush periodically to bound memory usage.
    const BATCH_FLUSH_EVERY: u64 = 1000;
    let mut batch = db.new_write_batch();
    let mut flushed_blocks: u64 = 0;

    // Block 0 (genesis) must always be kept: RepositoryManager::new() reads it at startup
    // to initialize the in-memory repository. Start deletion from block 1.
    for block_number in 1..cutoff {
        let block_key = be_block_key(block_number);

        // Read block hash from BlockNumberToHash.
        let Some(hash_bytes) = db
            .get_cf(RepositoryCF::BlockNumberToHash, &block_key)
            .with_context(|| format!("BlockNumberToHash read failed at block {block_number}"))?
        else {
            // Block not present (e.g., already deleted or genesis gap).
            continue;
        };

        // Tx/receipt/meta records are keyed by tx_hash and cannot be range-deleted here.
        // They are cleaned up in Phase 2 below via a TxMeta CF scan.
        batch.delete_cf(RepositoryCF::BlockData, &hash_bytes);
        batch.delete_cf(RepositoryCF::BlockNumberToHash, &block_key);

        if block_number % BATCH_FLUSH_EVERY == BATCH_FLUSH_EVERY - 1 {
            db.write(batch).context("failed to flush repository batch")?;
            batch = db.new_write_batch();
            flushed_blocks += BATCH_FLUSH_EVERY;
            info!("  ... deleted {flushed_blocks} blocks so far");
        }
    }

    db.write(batch).context("failed to write final repository batch")?;
    info!("Repository block-keyed records deleted for blocks 0..{cutoff}");

    // Phase 2: scan TxMeta to delete orphaned tx-hash-keyed records (Tx, TxReceipt, TxMeta).
    // Block-keyed deletion above removes BlockData + BlockNumberToHash but leaves Tx/receipt/meta
    // as dead data — they're keyed by tx_hash and can't be range-deleted by block number.
    // We scan all TxMeta entries, decode the block_number field, and delete any tx whose block
    // is below the cutoff.
    // Note: InitiatorAndNonceToHash entries are intentionally left as-is — cleaning them requires
    // decoding the full tx to extract initiator/nonce, and stale entries don't affect correctness
    // (they reference deleted txs, so lookups by sender+nonce just return a missing-tx result).
    // Note: LogBlocksByAddress + LogBlocksByTopic bitmaps are also left as-is — they require
    // reading each bitmap and clearing the bits for deleted block numbers, which is complex.
    // Stale bitmap bits only cause unnecessary work in eth_getLogs scans, not wrong results.
    let mut tx_hashes_to_delete: Vec<Vec<u8>> = Vec::new();
    for (k, v) in db.prefix_iterator_cf(RepositoryCF::TxMeta, &[]) {
        let block_number = tx_meta_block_number(&v)
            .context("failed to decode TxMeta entry")?;
        if block_number < cutoff {
            tx_hashes_to_delete.push(k.to_vec());
        }
    }

    const TX_BATCH_SIZE: usize = 10_000;
    info!(
        "Collected {} tx records to delete from Tx/TxReceipt/TxMeta CFs",
        tx_hashes_to_delete.len()
    );
    let mut tx_deleted: usize = 0;
    for chunk in tx_hashes_to_delete.chunks(TX_BATCH_SIZE) {
        let mut batch = db.new_write_batch();
        for tx_hash in chunk {
            batch.delete_cf(RepositoryCF::Tx, tx_hash);
            batch.delete_cf(RepositoryCF::TxReceipt, tx_hash);
            batch.delete_cf(RepositoryCF::TxMeta, tx_hash);
            tx_deleted += 1;
        }
        db.write(batch).context("failed to write tx deletion batch")?;
        if tx_deleted % 100_000 < TX_BATCH_SIZE {
            info!("  ... {tx_deleted} tx records deleted so far");
        }
    }
    info!("Repository trimmed: deleted {tx_deleted} tx records for blocks 0..{cutoff}");

    if compact {
        info!("Compacting repository column families...");
        for cf in [
            RepositoryCF::BlockData,
            RepositoryCF::BlockNumberToHash,
            RepositoryCF::Tx,
            RepositoryCF::TxReceipt,
            RepositoryCF::TxMeta,
        ] {
            db.compact_cf(cf);
        }
        info!("Repository compaction done");
    }
    Ok(())
}

// ── State full diffs trimmer ──────────────────────────────────────────────────

fn trim_state_diffs(db_path: &Path, keep_blocks: u64, dry_run: bool, compact: bool) -> anyhow::Result<()> {
    info!("Opening state_full_diffs at {:?}", db_path);
    let db = RocksDB::<StateDiffsCF>::new(db_path)
        .with_context(|| format!("failed to open state_full_diffs at {db_path:?}"))?;

    let latest_bytes = db
        .get_cf(StateDiffsCF::Meta, b"latest_block")
        .context("failed to read state_full_diffs Meta")?
        .context("state_full_diffs Meta latest_block missing — is DB empty?")?;
    let latest: u64 = u64::from_be_bytes(
        latest_bytes
            .as_slice()
            .try_into()
            .context("Meta latest_block has unexpected length")?,
    );

    if latest < keep_blocks {
        info!(
            "state_full_diffs latest={latest} < keep_blocks={keep_blocks}, nothing to trim"
        );
        return Ok(());
    }
    let cutoff = latest - keep_blocks;
    info!(
        "state_full_diffs: latest={latest}, cutoff={cutoff}; \
         for each storage key, keeping the most-recent write at or before block {cutoff}"
    );

    if dry_run {
        info!(
            "[dry-run] would compact state_full_diffs: \
             for each 32-byte key, delete all but the last write ≤ block {cutoff}"
        );
        return Ok(());
    }

    // Keys in the Data CF: hashed_key[32] ++ block_number_be[8].
    // We iterate forward (keys are lexicographically ordered, so all entries for a given
    // storage key appear in ascending block-number order), and for each storage key keep
    // only the last entry whose block_number ≤ cutoff.

    let mut keys_processed: u64 = 0;
    let mut entries_deleted: u64 = 0;

    // State for the streaming pass.
    let mut current_prefix: Option<[u8; 32]> = None;
    // The most recent pre-cutoff entry seen for `current_prefix`.
    // We keep this entry and delete all earlier ones for the same key.
    let mut last_pre_cutoff_key: Option<Vec<u8>> = None;
    // Earlier pre-cutoff entries for `current_prefix` that are queued for deletion.
    let mut to_delete: Vec<Vec<u8>> = Vec::new();

    const FLUSH_EVERY: u64 = 100_000;

    // Collect all keys to delete, then write in batches.
    // We separate iteration from writing because WriteBatch borrows from RocksDB,
    // and we can't hold both an iterator and a mutable borrow at the same time easily.
    let mut all_deletes: Vec<Vec<u8>> = Vec::new();

    for (k, _v) in db.prefix_iterator_cf(StateDiffsCF::Data, &[]) {
        if k.len() != 40 {
            bail!(
                "unexpected key length {} in state_full_diffs Data CF (expected 40)",
                k.len()
            );
        }
        let prefix: [u8; 32] = k[..32].try_into().unwrap();
        let block_num = u64::from_be_bytes(k[32..40].try_into().unwrap());

        if Some(prefix) != current_prefix {
            // Finalize the previous storage key: queue all candidates for deletion,
            // but keep last_pre_cutoff_key (the most recent write at or before cutoff).
            all_deletes.extend(to_delete.drain(..));
            last_pre_cutoff_key = None;
            current_prefix = Some(prefix);
            keys_processed += 1;
        }

        if block_num < cutoff {
            // Push the previous "last pre-cutoff" to the deletion queue.
            if let Some(prev_key) = last_pre_cutoff_key.take() {
                to_delete.push(prev_key);
            }
            last_pre_cutoff_key = Some(k.to_vec());
        }
        // Entries with block_num >= cutoff are kept unconditionally.
    }

    // Finalize the last storage key.
    all_deletes.extend(to_delete.drain(..));

    // Write deletions in chunks.
    info!("Collected {} state entries to delete", all_deletes.len());
    for chunk in all_deletes.chunks(FLUSH_EVERY as usize) {
        let mut batch = db.new_write_batch();
        for key in chunk {
            batch.delete_cf(StateDiffsCF::Data, key);
            entries_deleted += 1;
        }
        db.write(batch).context("failed to write state_diffs batch")?;
        info!("  ... {entries_deleted} state entries deleted so far");
    }

    info!(
        "state_full_diffs trimmed: scanned {keys_processed} storage keys, \
         deleted {entries_deleted} old entries"
    );

    if compact {
        info!("Compacting state_full_diffs...");
        db.compact_cf(StateDiffsCF::Data);
        info!("state_full_diffs compaction done");
    }
    Ok(())
}

// ── Tree GC ───────────────────────────────────────────────────────────────────
//
// The zkos_merkle_tree is an AmortizedLinkedListMT: a persistent (copy-on-write) trie where
// each version shares unchanged subtrees with prior versions via ChildRef.version pointers.
// Over thousands of blocks, 99%+ of nodes accumulate as unreachable historical data.
//
// GC: DFS from the latest-version root, following ChildRef.version pointers to collect all
// reachable NodeKeys. Delete everything else from the Tree CF.
//
// Format reference (from lib/merkle_tree/src/storage/serialization.rs):
//   NodeKey on disk: version[8 BE] || nibble_count[1] || index_on_level[8 BE]  = 17 bytes
//   ChildRef:        hash[32] || version LEB128
//   InternalNode:    len[1] || len × ChildRef
//   Root:            leaf_count LEB128 || InternalNode
//   Manifest at key [0]: version_count LEB128 || tag_count LEB128 || (key LEB128-str, value LEB128-str)*

fn leb128_read_u64(buf: &mut &[u8]) -> anyhow::Result<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = buf.first().context("unexpected EOF in LEB128")?;
        *buf = &buf[1..];
        result |= ((*byte & 0x7f) as u64) << shift;
        if *byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 64 {
            bail!("LEB128 u64 overflow");
        }
    }
}

fn leb128_read_str<'a>(buf: &mut &'a [u8]) -> anyhow::Result<&'a str> {
    let len = leb128_read_u64(buf)? as usize;
    if buf.len() < len {
        bail!("unexpected EOF reading LEB128 string (need {len}, have {})", buf.len());
    }
    let s = std::str::from_utf8(&buf[..len]).context("non-UTF8 string in manifest")?;
    *buf = &buf[len..];
    Ok(s)
}

// Returns (version_count, tree_depth, internal_node_depth).
fn parse_tree_manifest(bytes: &[u8]) -> anyhow::Result<(u64, u8, u8)> {
    let buf = &mut &bytes[..];
    let version_count = leb128_read_u64(buf)?;
    let tag_count = leb128_read_u64(buf)?;

    let mut depth: Option<u8> = None;
    let mut internal_node_depth: Option<u8> = None;
    for _ in 0..tag_count {
        let key = leb128_read_str(buf)?;
        let value = leb128_read_str(buf)?;
        match key {
            "depth" => depth = Some(value.parse().context("bad depth tag")?),
            "internal_node_depth" => {
                internal_node_depth = Some(value.parse().context("bad internal_node_depth tag")?)
            }
            _ => {}
        }
    }
    Ok((
        version_count,
        depth.context("manifest missing depth tag")?,
        internal_node_depth.context("manifest missing internal_node_depth tag")?,
    ))
}

struct TreeNodeKey {
    version: u64,
    nibble_count: u8,
    index_on_level: u64,
}

impl TreeNodeKey {
    fn root(version: u64) -> Self {
        Self { version, nibble_count: 0, index_on_level: 0 }
    }

    fn as_db_key(&self) -> [u8; 17] {
        let mut buf = [0u8; 17];
        buf[..8].copy_from_slice(&self.version.to_be_bytes());
        buf[8] = self.nibble_count;
        buf[9..].copy_from_slice(&self.index_on_level.to_be_bytes());
        buf
    }
}

// Returns (hash[32], version) for each child in an internal node.
fn parse_internal_node_children(buf: &mut &[u8]) -> anyhow::Result<Vec<([u8; 32], u64)>> {
    let len = *buf.first().context("EOF reading internal node child count")? as usize;
    *buf = &buf[1..];
    let mut children = Vec::with_capacity(len);
    for _ in 0..len {
        if buf.len() < 32 {
            bail!("EOF reading child hash");
        }
        let hash: [u8; 32] = buf[..32].try_into().unwrap();
        *buf = &buf[32..];
        let version = leb128_read_u64(buf)?;
        children.push((hash, version));
    }
    Ok(children)
}

fn trim_tree(db_path: &Path, keep_versions: u64, dry_run: bool, compact: bool) -> anyhow::Result<()> {
    const MANIFEST_KEY: &[u8] = &[0];
    const NODE_KEY_LEN: usize = 17;

    info!("Opening zkos_merkle_tree at {:?}", db_path);
    let db = RocksDB::<MerkleTreeColumnFamily>::new(db_path)
        .with_context(|| format!("failed to open zkos_merkle_tree at {db_path:?}"))?;

    let manifest_bytes = db
        .get_cf(MerkleTreeColumnFamily::Tree, MANIFEST_KEY)
        .context("failed to read manifest")?
        .context("tree manifest missing — is tree DB empty?")?;

    let (version_count, tree_depth, internal_node_depth) =
        parse_tree_manifest(&manifest_bytes)?;

    if version_count == 0 {
        info!("Tree has no versions, nothing to GC");
        return Ok(());
    }

    let latest_version = version_count - 1;
    // leaf_nibbles = ceil(tree_depth / internal_node_depth)
    let leaf_nibbles = tree_depth.div_ceil(internal_node_depth);
    let max_node_children = 1u64 << internal_node_depth;

    info!(
        "Tree: latest_version={latest_version}, depth={tree_depth}, \
         internal_node_depth={internal_node_depth}, leaf_nibbles={leaf_nibbles}, \
         max_node_children={max_node_children}"
    );

    if dry_run {
        // Just count total nodes and estimate GC savings.
        let total_nodes = db.prefix_iterator_cf(MerkleTreeColumnFamily::Tree, &[]).count();
        info!(
            "[dry-run] tree has {total_nodes} entries (including manifest); \
             GC would keep only nodes reachable from version {latest_version}"
        );
        return Ok(());
    }

    // DFS from every version in the kept range to collect all reachable NodeKeys.
    //
    // The tree is a COW trie: each version's root is a separate DB entry, and nodes changed
    // in a block have new copies while old nodes remain (referenced by older version roots).
    // The tree_manager processes blocks asynchronously and needs the full tree state at each
    // version in the kept range (e.g., to apply block N+1 it reads nodes from version N).
    // DFS from only the latest version leaves intermediate version roots and their
    // "shadowed" (since-overwritten) nodes deleted, breaking the tree_manager.
    //
    // Because the HashSet deduplicates shared nodes (most nodes are unchanged across versions),
    // the incremental cost per version is proportional to state changes in that block only.
    let cutoff_version = latest_version.saturating_sub(keep_versions);
    let mut reachable: HashSet<[u8; NODE_KEY_LEN]> = HashSet::new();

    // Always preserve the genesis root (version 0): the server calls root_info(0) at startup
    // to load genesis root hash + leaf count. Only this single node is needed.
    reachable.insert(TreeNodeKey::root(0).as_db_key());

    let mut visited_count = 0u64;

    // Start DFS from the latest version first (fastest deduplication baseline), then
    // work backwards through older kept versions to pick up shadowed nodes.
    for start_version in (cutoff_version..=latest_version).rev() {
        let mut stack: Vec<TreeNodeKey> = vec![TreeNodeKey::root(start_version)];

        while let Some(node_key) = stack.pop() {
            let db_key = node_key.as_db_key();
            if !reachable.insert(db_key) {
                continue; // already visited (shared with a newer version's DFS)
            }
            visited_count += 1;
            if visited_count % 100_000 == 0 {
                info!("  ... traversed {visited_count} reachable nodes so far");
            }

            if node_key.nibble_count >= leaf_nibbles {
                continue; // leaf node — no children to follow
            }

            let raw = match db.get_cf(MerkleTreeColumnFamily::Tree, &db_key)? {
                Some(v) => v,
                None => continue, // virtual empty subtree (no node stored)
            };

            // Root (nibble_count == 0) has leaf_count LEB128 prepended; skip it.
            let buf = &mut raw.as_slice();
            if node_key.nibble_count == 0 {
                leb128_read_u64(buf)?; // skip leaf_count
            }
            let children = parse_internal_node_children(buf)?;

            for (i, (_hash, child_version)) in children.iter().enumerate() {
                let child_key = TreeNodeKey {
                    version: *child_version,
                    nibble_count: node_key.nibble_count + 1,
                    index_on_level: node_key.index_on_level * max_node_children + i as u64,
                };
                stack.push(child_key);
            }
        }
    }

    info!(
        "Tree GC: {visited_count} reachable nodes found across versions {cutoff_version}..={latest_version}"
    );

    // Scan all Tree CF entries (excluding the manifest) and delete unreachable ones.
    let mut to_delete: Vec<Vec<u8>> = Vec::new();
    for (raw_key, _) in db.prefix_iterator_cf(MerkleTreeColumnFamily::Tree, &[]) {
        if raw_key.len() != NODE_KEY_LEN {
            continue; // manifest key ([0]) or unexpected — skip
        }
        let key_arr: [u8; NODE_KEY_LEN] = raw_key.as_ref().try_into().unwrap();
        if !reachable.contains(&key_arr) {
            to_delete.push(raw_key.to_vec());
        }
    }

    info!("Tree GC: deleting {} unreachable nodes", to_delete.len());

    for chunk in to_delete.chunks(50_000) {
        let mut batch = db.new_write_batch();
        for key in chunk {
            batch.delete_cf(MerkleTreeColumnFamily::Tree, key);
        }
        db.write(batch).context("failed writing tree GC deletion batch")?;
    }

    info!("Tree GC complete");

    if compact {
        info!("Compacting Tree CF...");
        db.compact_cf(MerkleTreeColumnFamily::Tree);
        info!("Tree CF compaction done");
    }
    Ok(())
}

// ── Node dir resolution & cleanup ───────────────────────────────────────────────

/// Conventional name of the node DB directory inside a snapshot parent directory.
const NODE_DIR_NAME: &str = "node1";

/// Sub-DBs that must be present (with a CURRENT file) for a directory to be treated as a node DB.
const REQUIRED_SUB_DBS: &[&str] = &["block_replay_wal", "repository", "state_full_diffs"];

fn looks_like_node_dir(dir: &Path) -> bool {
    REQUIRED_SUB_DBS
        .iter()
        .all(|sub| dir.join(sub).join("CURRENT").exists())
}

/// Resolves `--db-dir` to the actual node DB directory.
///
/// Returns `(node_dir, cleanup_parent)`:
/// - If `--db-dir` is itself a node DB directory, returns it with no cleanup parent.
/// - If `--db-dir` is a snapshot parent directory (contains `node1/`), returns the node dir and
///   the parent (whose non-`node1` siblings may be cleaned up).
fn resolve_node_dir(db_dir: &Path) -> anyhow::Result<(PathBuf, Option<PathBuf>)> {
    if looks_like_node_dir(db_dir) {
        return Ok((db_dir.to_path_buf(), None));
    }
    let node_dir = db_dir.join(NODE_DIR_NAME);
    if looks_like_node_dir(&node_dir) {
        return Ok((node_dir, Some(db_dir.to_path_buf())));
    }
    bail!(
        "Could not find a node DB under {db_dir:?}.\n\
         Expected either:\n\
         - the node DB directory itself (containing {REQUIRED_SUB_DBS:?}), or\n\
         - a parent directory containing a `{NODE_DIR_NAME}/` subdirectory.\n\
         Found neither (no CURRENT files in the expected sub-DBs)."
    );
}

/// Deletes every entry in `parent` except `node1/`. Used to strip generated data
/// (e.g. `fri_proofs/`, `block_dumps/`) that the server does not need on startup.
fn cleanup_parent_dir(parent: &Path, dry_run: bool) -> anyhow::Result<()> {
    info!(
        "Cleaning up {parent:?}: removing everything except {NODE_DIR_NAME}/"
    );
    let mut removed = 0u64;
    for entry in std::fs::read_dir(parent).with_context(|| format!("read_dir {parent:?}"))? {
        let entry = entry?;
        if entry.file_name() == NODE_DIR_NAME {
            continue;
        }
        let path = entry.path();
        if dry_run {
            info!("  [dry-run] would remove {path:?}");
            removed += 1;
            continue;
        }
        let file_type = entry
            .file_type()
            .with_context(|| format!("file_type {path:?}"))?;
        if file_type.is_dir() {
            std::fs::remove_dir_all(&path).with_context(|| format!("remove_dir_all {path:?}"))?;
        } else {
            // Plain file or symlink — remove the entry itself (not a symlink's target).
            std::fs::remove_file(&path).with_context(|| format!("remove_file {path:?}"))?;
        }
        info!("  removed {path:?}");
        removed += 1;
    }
    if removed == 0 {
        info!("  nothing to clean up (only {NODE_DIR_NAME}/ present)");
    }
    Ok(())
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Read-only query mode: report the batch covering a block, then exit (no trimming).
    if let Some(maybe_block) = cli.print_batch_for_block {
        if !cli.db_dir.exists() {
            bail!("--db-dir {:?} does not exist", cli.db_dir);
        }
        let (node_dir, _) = resolve_node_dir(&cli.db_dir)?;
        let target_block = match maybe_block {
            Some(b) => b,
            None => {
                let tip = snapshot_tip(&node_dir)?;
                info!("No block given; using snapshot tip (min height across sub-DBs) = {tip}");
                tip
            }
        };
        return print_batch_for_block(&node_dir, target_block);
    }

    if cli.dry_run {
        info!("DRY RUN — no data will be modified");
    }

    // The server replays from the oldest block it still needs on startup, which is dominated
    // by `last_l1_executed_block` — how far L1 execution lags behind the sequencer tip. That
    // value is not in the snapshot (it's re-fetched from L1 each startup), so we can't compute
    // the exact minimum. Instead we keep a generous fixed margin and refuse values small enough
    // to plausibly cut below the execution lag, which would yield a snapshot that panics with
    // "Unless it's a new chain, replay record must exist".
    const RECOMMENDED_MIN_KEEP_BLOCKS: u64 = 1000;
    const ABSOLUTE_MIN_KEEP_BLOCKS: u64 = 10; // the server's min_blocks_to_replay default

    if cli.keep_blocks < ABSOLUTE_MIN_KEEP_BLOCKS {
        bail!(
            "--keep-blocks must be at least {ABSOLUTE_MIN_KEEP_BLOCKS} \
             (the server's min_blocks_to_replay default)"
        );
    }
    if cli.keep_blocks < RECOMMENDED_MIN_KEEP_BLOCKS && !cli.allow_small_keep {
        bail!(
            "--keep-blocks={} is below the recommended safe floor of {RECOMMENDED_MIN_KEEP_BLOCKS}.\n\
             \n\
             On startup the server replays from `last_l1_executed_block`, which can lag the WAL \
             tip by hundreds of blocks (e.g. ~233 in a recent stage snapshot). That number is not \
             stored in the snapshot, so the trimmer cannot verify your value is safe. If --keep-blocks \
             is smaller than the execution lag, the trimmed snapshot will look fine but panic on \
             startup with \"Unless it's a new chain, replay record must exist\".\n\
             \n\
             Use --keep-blocks 2000 (the default) for a safe margin, or pass --allow-small-keep \
             if you are certain the L1 execution lag for this snapshot is below {}.",
            cli.keep_blocks, cli.keep_blocks
        );
    }

    let db_dir = &cli.db_dir;
    if !db_dir.exists() {
        bail!("--db-dir {:?} does not exist", db_dir);
    }

    // Accept either the node DB directory itself or a snapshot parent dir containing `node1/`.
    let (node_dir, cleanup_parent) = resolve_node_dir(db_dir)?;
    info!("Using node DB directory {node_dir:?}");

    // Strip generated data (fri_proofs/, block_dumps/, …) when given a parent dir.
    if cli.cleanup() {
        match &cleanup_parent {
            Some(parent) => cleanup_parent_dir(parent, cli.dry_run)?,
            None => info!(
                "--db-dir points directly at the node DB; no sibling cleanup (pass the parent \
                 dir to also strip {NODE_DIR_NAME}/'s siblings)"
            ),
        }
    }

    let compact = cli.compact();

    // A DB copied from a live (or crashed) node can have its sub-DBs at different block heights.
    // Align WAL + state down to the common consistent height before trimming, otherwise the server
    // panics on startup with a "historical write discrepancy".
    if cli.normalize() {
        normalize_snapshot(&node_dir, cli.dry_run)?;
    }

    trim_wal(
        &node_dir.join("block_replay_wal"),
        cli.keep_blocks,
        cli.dry_run,
        compact,
    )?;
    trim_repository(
        &node_dir.join("repository"),
        cli.keep_blocks,
        cli.dry_run,
        compact,
    )?;
    trim_state_diffs(
        &node_dir.join("state_full_diffs"),
        cli.keep_blocks,
        cli.dry_run,
        compact,
    )?;

    if cli.trim_tree() {
        let tree_path = node_dir.join("tree");
        if !tree_path.join("CURRENT").exists() {
            bail!(
                "Expected zkos_merkle_tree at {tree_path:?} but found no CURRENT file. \
                 Is --db-dir correct?"
            );
        }
        trim_tree(&tree_path, cli.keep_blocks, cli.dry_run, compact)?;
    }

    if cli.dry_run {
        info!("Dry run complete.");
    } else if !compact {
        info!(
            "Trim complete. Data is logically deleted but disk space is not yet reclaimed.\n\
             Drop --no-compact to force compaction now, or the server will compact on next start."
        );
    } else {
        info!("Trim and compaction complete.");
    }

    Ok(())
}
