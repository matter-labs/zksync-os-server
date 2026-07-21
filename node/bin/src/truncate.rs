//! `truncate-to` — the disaster-fork chain truncation tool.
//!
//! Discards the chain suffix above an agreed height N on a *stopped* node of
//! any role, exporting the doomed blocks to a tombstone archive first. This is
//! the mechanical half of a disaster hardfork (see `docs/src/consensus/`): the
//! decision to fork, the L1 batch revert, the fork configuration, and the
//! coordinated restart are the operators' runbook — this tool only makes the
//! local chain end exactly at N, verifiably.
//!
//! What it touches: the block-replay write-ahead log (truncated, pointer
//! first), the state diffs (truncated), and the repositories (rolled back —
//! a forked node must not keep serving discarded blocks and receipts over
//! RPC). What it deliberately does not touch: the finality store (the
//! abandoned era's certificates are the permanent, auditable record of what
//! was overridden), the merkle tree (it truncates itself when replay hands it
//! an older block — the same rewind path every rebuild uses), preimages
//! (content-addressed), and the consensus engine state (the fork runbook
//! clears it separately; the era guards refuse to start otherwise).
//!
//! Guards, all fail-closed:
//! - the node must be stopped (opening the databases fails while it runs);
//! - N must be at or below the local chain tip;
//! - N must be at or above the last block of the last batch *executed* on L1,
//!   read from L1 directly — past that line there is no going back for anyone
//!   (reversing L1-executed state is an L1 emergency-upgrade scenario, out of
//!   scope by decision). The local batch store only maps the L1 batch number
//!   to a block; if it cannot, the tool refuses rather than guesses.
//! - the state backend must be `FullDiffs`: the compacted backend cannot
//!   replay below its compaction start, so truncation there would strand the
//!   node.

use crate::config::{Config, StateBackendConfig};
use crate::provider::{ProviderKind, build_node_provider};
use anyhow::Context as _;
use std::path::PathBuf;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_state_full_diffs::FullDiffsState;
use zksync_os_storage::db::{BlockReplayStorage, ExecutedBatchStorage, RepositoryDb};
use zksync_os_storage_api::{ReadBatch as _, ReadReplay as _};

/// One tombstoned block in the manifest: enough to locate and audit it.
#[derive(serde::Serialize)]
struct TombstonedBlock {
    number: u64,
    hash: String,
    /// The consensus digest, when this node had one recorded (validators and
    /// observers do; a plain external node has no finality store).
    consensus_digest: Option<String>,
    transactions: usize,
}

#[derive(serde::Serialize)]
struct TombstoneManifest {
    truncated_to: u64,
    hash_at_truncation_point: String,
    old_tip: u64,
    l1_executed_floor_block: u64,
    blocks: Vec<TombstonedBlock>,
}

pub async fn run_truncate(
    config: Config,
    to_block: u64,
    tombstone_dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(
            config.general_config.state_backend,
            StateBackendConfig::FullDiffs
        ),
        "truncation requires the FullDiffs state backend: the compacted backend \
         cannot replay below its compaction start, so a truncated node could \
         never rebuild its derived state"
    );
    let rocks = config.general_config.rocks_db_path.clone();
    let chain_id = config
        .genesis_config
        .chain_id
        .context("`genesis.chain_id` is required to open the WAL and verify the L1-executed floor")?;

    // Opening the databases doubles as the running-node guard: RocksDB holds
    // an exclusive lock per database while the node lives.
    let wal = BlockReplayStorage::new_without_genesis(
        &rocks.join(crate::BLOCK_REPLAY_WAL_DB_NAME),
        chain_id,
    );
    let tip = wal.latest_record();
    anyhow::ensure!(
        to_block <= tip,
        "cannot truncate to {to_block}: the local chain ends at {tip}"
    );

    // The L1-executed floor (verified against L1 directly, not against local
    // watermarks — a lagging local view must never make the guard permissive).
    let l1_provider = build_node_provider(
        &config.l1_provider_config,
        config.l1_watcher_config.poll_interval,
        config.l1_watcher_config.finalized_poll_interval,
        config.l1_watcher_config.logs_cache_capacity,
        ProviderKind::L1,
    )
    .await;
    let bridgehub_address = config
        .genesis_config
        .bridgehub_address
        .context("`genesis.bridgehub_address` is required to verify the L1-executed floor")?;
    // The *latest* L1 view, deliberately: an execution that is not yet
    // L1-finalized may still become final, so it already binds the floor.
    let l1_state =
        L1State::fetch_with_finality(false, l1_provider.clone(), bridgehub_address, chain_id)
            .await
            .context("failed to fetch L1 state for the executed-batch floor")?;
    let floor_block = if l1_state.last_executed_batch == 0 {
        0
    } else {
        // `batch` is the directory the node itself opens this store at.
        let batches = ExecutedBatchStorage::new(&rocks.join("batch"));
        batches
            .get_batch_by_number(l1_state.last_executed_batch)?
            .with_context(|| {
                format!(
                    "L1 reports batch {} executed, but this node has not observed it — \
                     it is behind L1-executed state and cannot participate in a fork \
                     until it syncs",
                    l1_state.last_executed_batch
                )
            })?
            .last_block_number()
    };
    anyhow::ensure!(
        to_block >= floor_block,
        "cannot truncate to {to_block}: block {floor_block} is the last block of the \
         last batch executed on L1 (batch {}) — L1-executed state is irreversible; \
         reversing it would be an L1 emergency upgrade, a different disaster",
        l1_state.last_executed_batch,
    );

    if to_block == tip {
        tracing::info!(to_block, "the chain already ends at the requested height");
        return Ok(());
    }

    // The tombstone: every doomed block, in the released wire encoding
    // (decode-forever), plus a manifest for the postmortem and for any
    // external consumer that saw the old tip.
    let tombstone = tombstone_dir.unwrap_or_else(|| rocks.join(format!("tombstone-{to_block}")));
    anyhow::ensure!(
        !tombstone.exists(),
        "tombstone directory {} already exists; refusing to overwrite an \
         existing export",
        tombstone.display()
    );
    std::fs::create_dir_all(&tombstone).context("failed to create the tombstone directory")?;

    let finality_dir = rocks.join("finality");
    let finality = finality_dir
        .exists()
        .then(|| zksync_os_consensus_execution::FinalityStore::open(&finality_dir))
        .transpose()
        .context("failed to open the finality store for digest export")?;

    let mut blocks = Vec::with_capacity((tip - to_block) as usize);
    for number in (to_block + 1)..=tip {
        let record = wal
            .get_replay_record(number)
            .with_context(|| format!("the write-ahead log has no record for block {number}"))?;
        let hash = wal.canonical_block_hash(number);
        let transactions = record.transactions.len();
        let wire: zksync_os_wire::replays::v3::ReplayRecord = record.into();
        let encoded = alloy_rlp::encode(&wire);
        std::fs::write(tombstone.join(format!("{number}.replay")), &encoded)
            .with_context(|| format!("failed to write the tombstone record for {number}"))?;
        let consensus_digest = finality
            .as_ref()
            .and_then(|store| store.digest_at_height(number).ok().flatten())
            .map(alloy::hex::encode);
        blocks.push(TombstonedBlock {
            number,
            hash: format!("{hash:?}"),
            consensus_digest,
            transactions,
        });
    }
    let hash_at_truncation_point = format!("{:?}", wal.canonical_block_hash(to_block));
    let manifest = TombstoneManifest {
        truncated_to: to_block,
        hash_at_truncation_point: hash_at_truncation_point.clone(),
        old_tip: tip,
        l1_executed_floor_block: floor_block,
        blocks,
    };
    std::fs::write(
        tombstone.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).expect("manifest serializes"),
    )
    .context("failed to write the tombstone manifest")?;
    tracing::info!(
        exported = tip - to_block,
        tombstone = %tombstone.display(),
        "tombstone archive written"
    );

    // Consensus engine state (vote journals, marshal's archives and delivery
    // marker) that recorded progress past the truncation point must never run
    // again — marshal would resume delivery above the new tip. Flag the
    // directory before the cuts (a crash in between leaves the node stricter,
    // matching the cut ordering below): startup refuses to run consensus over
    // the flag, and the runbook's clear-the-engine-state step removes the flag
    // together with the state it poisons.
    let engine_dir = rocks.join("consensus");
    let engine_state_is_fresh =
        crate::consensus_engine_state_is_fresh(&engine_dir).with_context(|| {
            format!(
                "failed to inspect consensus engine state at {}",
                engine_dir.display()
            )
        })?;
    if !engine_state_is_fresh {
        std::fs::write(
            crate::consensus::truncation_flag_path(&engine_dir),
            to_block.to_string(),
        )
        .context("failed to flag the consensus engine state as pre-truncation")?;
    }

    // The cuts, ordered so that a crash between them leaves the node
    // *stricter* than intended, never looser: the WAL first (its pointer is
    // what every startup consults), then the state diffs, then the
    // repositories. Re-running the tool with the same N finishes any
    // interrupted step — every cut is idempotent.
    wal.truncate_to(to_block);
    tracing::info!(to_block, "write-ahead log truncated");
    FullDiffsState::open_existing(rocks.clone())
        .context("failed to open the state for truncation")?
        .truncate_to(to_block)
        .context("failed to truncate the state diffs")?;
    tracing::info!(to_block, "state diffs truncated");
    RepositoryDb::open_existing(&rocks.join("repository"))
        .context("failed to open the repositories for rollback")?
        .rollback(to_block)
        .context("failed to roll back the repositories")?;
    tracing::info!(to_block, "repositories rolled back");

    tracing::info!(
        to_block,
        hash = %hash_at_truncation_point,
        "chain truncated. Next steps for a disaster fork: clear the consensus \
         engine state directory, deploy the fork configuration \
         (`consensus.genesis_height = {to_block}`, a bumped \
         `consensus.protocol_version`, and `consensus.acknowledge_fork = \
         \"{to_block}:{hash_at_truncation_point}\"`), and restart — the settler \
         last, after the L1 batch revert is verified"
    );
    Ok(())
}
