//! `chain-tip` — prints the height at which a stopped node's chain ends, and
//! the block hash at that height.
//!
//! Several consensus operations need this pair: a migration sets
//! `consensus.genesis_height` to the drained sequencer's tip, a rollback
//! picks the validator with the highest tip as the survivor, and a disaster
//! fork sets `consensus.acknowledge_fork = "<height>:<hash>"` — the exact
//! string this command prints. The pair must be read from the stopped node's
//! database: a height read over RPC before the stop can be outdated, because
//! the node keeps producing blocks until the moment it stops.
//!
//! Read-only: nothing is truncated or written. Opening the database also
//! guards against a running node — RocksDB holds an exclusive lock per
//! database while the node runs.

use crate::config::Config;
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use zksync_os_storage::db::BlockReplayStorage;
use zksync_os_storage_api::ReadReplay as _;

/// Reads the chain tip — the height the write-ahead log ends at and the
/// canonical block hash at that height — from a stopped node's database.
pub fn read_chain_tip(config: &Config) -> anyhow::Result<(BlockNumber, BlockHash)> {
    let chain_id = config
        .genesis_config
        .chain_id
        .context("`genesis.chain_id` is required to open the write-ahead log")?;
    let wal_path = config
        .general_config
        .rocks_db_path
        .join(crate::BLOCK_REPLAY_WAL_DB_NAME);
    // Checked before opening: RocksDB creates a missing database on open, so a
    // mistyped `general.rocks_db_path` must fail here instead of creating an
    // empty database and reporting a wrong tip.
    anyhow::ensure!(
        wal_path.is_dir(),
        "no write-ahead log at {} — is `general.rocks_db_path` pointing at the node's database?",
        wal_path.display()
    );
    let wal = BlockReplayStorage::new_without_genesis(&wal_path, chain_id);
    let tip = wal.latest_record();
    Ok((tip, wal.canonical_block_hash(tip)))
}
