use crate::config::{Config, RollbackL2BlockConfig, StateBackendConfig};
use crate::{
    BATCH_DB_NAME, BLOCK_REPLAY_WAL_DB_NAME, PRIORITY_TREE_DB_NAME, REPOSITORY_DB_NAME,
    STATE_TREE_DB_NAME,
};
use anyhow::Context;
use std::num::NonZeroU32;
use std::path::Path;
use std::time::Duration;
use zksync_os_merkle_tree::{MerkleTree, MerkleTreeColumnFamily, RocksDBWrapper};
use zksync_os_rocksdb::{RocksDB, RocksDBOptions, StalledWritesRetries};
use zksync_os_state::StateHandle;
use zksync_os_state_full_diffs::FullDiffsState;
use zksync_os_storage::db::{BlockReplayStorage, ExecutedBatchStorage, RepositoryDb};

const RECOVERY_MAX_OPEN_FILES: u32 = 512;
const DEFAULT_DB_SUBDIR: &str = "node1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOutcome {
    NotConfigured,
    Applied,
    AlreadyApplied,
}

pub fn apply(config: &Config) -> anyhow::Result<RecoveryOutcome> {
    let Some(rollback) = &config.storage_recovery_config.rollback_l2_block else {
        return Ok(RecoveryOutcome::NotConfigured);
    };
    anyhow::ensure!(
        !config.consensus_config.enabled,
        "storage recovery rollback requires `consensus.enabled=false`"
    );
    anyhow::ensure!(
        !config.general_config.ephemeral,
        "storage recovery rollback requires `general.ephemeral=false`"
    );

    let from_block = rollback.number;
    let last_block_to_keep = from_block
        .checked_sub(1)
        .context("storage recovery rollback block must be greater than 0")?;

    let db_root = resolve_db_root(&config.general_config.rocks_db_path)?;
    let wal_path = db_root.join(BLOCK_REPLAY_WAL_DB_NAME);
    let replay_storage =
        BlockReplayStorage::try_new_without_genesis_with_options(&wal_path, recovery_db_options())
            .with_context(|| {
                format!("failed to open block replay WAL at {}", wal_path.display())
            })?;
    let target_was_present = verify_wal_guard(&replay_storage, rollback, last_block_to_keep)?;

    tracing::warn!(
        from_block,
        last_block_to_keep,
        target_hash = ?rollback.hash,
        parent_hash = ?rollback.parent_hash,
        "applying startup L2 storage rollback"
    );

    rollback_repository(&db_root, last_block_to_keep)?;
    rollback_tree(
        &db_root.join(STATE_TREE_DB_NAME),
        last_block_to_keep,
        from_block,
    )?;
    rollback_state(
        &db_root,
        config.general_config.state_backend,
        last_block_to_keep,
    )?;
    rollback_batches(&db_root, from_block)?;
    rollback_priority_tree(&db_root, last_block_to_keep)?;
    replay_storage.rollback(last_block_to_keep)?;

    let outcome = if target_was_present {
        RecoveryOutcome::Applied
    } else {
        RecoveryOutcome::AlreadyApplied
    };
    tracing::warn!(
        from_block,
        last_block_to_keep,
        ?outcome,
        "startup L2 storage rollback completed"
    );
    Ok(outcome)
}

fn resolve_db_root(configured_path: &Path) -> anyhow::Result<std::path::PathBuf> {
    let direct_wal_path = configured_path.join(BLOCK_REPLAY_WAL_DB_NAME);
    if direct_wal_path.exists() {
        return Ok(configured_path.to_path_buf());
    }

    let nested_path = configured_path.join(DEFAULT_DB_SUBDIR);
    let nested_wal_path = nested_path.join(BLOCK_REPLAY_WAL_DB_NAME);
    if nested_wal_path.exists() {
        tracing::info!(
            configured_path = %configured_path.display(),
            resolved_path = %nested_path.display(),
            "storage recovery resolved mounted DB parent to RocksDB root"
        );
        return Ok(nested_path);
    }

    anyhow::bail!(
        "cannot apply storage recovery: block replay WAL path does not exist at {} or {}",
        direct_wal_path.display(),
        nested_wal_path.display()
    );
}

fn recovery_db_options() -> RocksDBOptions {
    RocksDBOptions {
        max_open_files: NonZeroU32::new(RECOVERY_MAX_OPEN_FILES),
        ..RocksDBOptions::default()
    }
}

fn recovery_tree_db_options() -> RocksDBOptions {
    RocksDBOptions {
        block_cache_capacity: Some(128 << 20),
        include_indices_and_filters_in_block_cache: false,
        large_memtable_capacity: Some(256 << 20),
        stalled_writes_retries: StalledWritesRetries::new(Duration::from_secs(10)),
        max_open_files: NonZeroU32::new(RECOVERY_MAX_OPEN_FILES),
    }
}

fn ensure_db_path_exists(path: &Path, db_name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        path.exists(),
        "{db_name} DB path does not exist: {}",
        path.display()
    );
    Ok(())
}

fn verify_wal_guard(
    replay_storage: &BlockReplayStorage,
    rollback: &RollbackL2BlockConfig,
    last_block_to_keep: u64,
) -> anyhow::Result<bool> {
    let latest = replay_storage
        .try_latest_record()
        .context("cannot apply storage recovery: block replay WAL is empty")?;
    anyhow::ensure!(
        latest >= last_block_to_keep,
        "cannot apply storage recovery for block {}; WAL latest block is {latest}, so parent block {} cannot be verified",
        rollback.number,
        last_block_to_keep
    );

    let actual_parent_hash = replay_storage
        .canonical_hash(last_block_to_keep)?
        .with_context(|| format!("missing canonical hash for parent block {last_block_to_keep}"))?;
    anyhow::ensure!(
        actual_parent_hash == rollback.parent_hash,
        "storage recovery parent hash mismatch for block {last_block_to_keep}: expected {}, got {}",
        rollback.parent_hash,
        actual_parent_hash
    );

    if latest >= rollback.number {
        let actual_hash = replay_storage
            .canonical_hash(rollback.number)?
            .with_context(|| {
                format!(
                    "missing canonical hash for target block {}",
                    rollback.number
                )
            })?;
        anyhow::ensure!(
            actual_hash == rollback.hash,
            "storage recovery target hash mismatch for block {}: expected {}, got {}",
            rollback.number,
            rollback.hash,
            actual_hash
        );
        Ok(true)
    } else {
        tracing::info!(
            latest,
            target_block = rollback.number,
            "target block is already absent from WAL; completing recovery for the remaining DBs"
        );
        Ok(false)
    }
}

fn rollback_repository(db_root: &Path, last_block_to_keep: u64) -> anyhow::Result<()> {
    let repository_path = db_root.join(REPOSITORY_DB_NAME);
    ensure_db_path_exists(&repository_path, "repository")?;
    let repository =
        RepositoryDb::new_without_genesis_with_options(&repository_path, recovery_db_options())
            .with_context(|| {
                format!(
                    "failed to open repository DB at {}",
                    repository_path.display()
                )
            })?;
    let latest = repository.latest_block_number();
    anyhow::ensure!(
        latest >= last_block_to_keep,
        "cannot roll back repository DB to block {last_block_to_keep}; latest repository block is {latest}"
    );
    repository.rollback(last_block_to_keep)?;
    Ok(())
}

fn rollback_tree(tree_path: &Path, last_block_to_keep: u64, from_block: u64) -> anyhow::Result<()> {
    ensure_db_path_exists(tree_path, "Merkle tree")?;
    let db: RocksDB<MerkleTreeColumnFamily> =
        RocksDB::with_options(tree_path, recovery_tree_db_options())
            .with_context(|| format!("failed to open Merkle tree DB at {}", tree_path.display()))?;
    let mut tree = MerkleTree::new(RocksDBWrapper::from(db))?;
    let latest = tree
        .latest_version()?
        .with_context(|| format!("Merkle tree DB is uninitialized: {}", tree_path.display()))?;
    anyhow::ensure!(
        latest >= last_block_to_keep,
        "cannot roll back Merkle tree to block {last_block_to_keep}; latest tree version is {latest}"
    );
    tree.truncate_recent_versions(from_block)?;
    Ok(())
}

fn rollback_state(
    db_root: &Path,
    state_backend: StateBackendConfig,
    last_block_to_keep: u64,
) -> anyhow::Result<()> {
    match state_backend {
        StateBackendConfig::FullDiffs => FullDiffsState::rollback_db_with_options(
            db_root.to_path_buf(),
            last_block_to_keep,
            recovery_db_options(),
        ),
        StateBackendConfig::Compacted => {
            StateHandle::ensure_rollback_db_possible(db_root.to_path_buf(), last_block_to_keep)
        }
    }
}

fn rollback_batches(db_root: &Path, from_block: u64) -> anyhow::Result<()> {
    let batch_path = db_root.join(BATCH_DB_NAME);
    let batch_storage = ExecutedBatchStorage::new(&batch_path);
    batch_storage.rollback_from_l2_block(from_block)
}

fn rollback_priority_tree(db_root: &Path, last_block_to_keep: u64) -> anyhow::Result<()> {
    zksync_os_priority_tree::rollback_cached_tree(
        &db_root.join(PRIORITY_TREE_DB_NAME),
        last_block_to_keep,
    )
}
