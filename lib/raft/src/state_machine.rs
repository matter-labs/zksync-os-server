//! OpenRaft state-machine implementation for replay-record based application state.
//!
//! `apply()` is invoked when a Raft log is canonized (accepted by a quorum):
//! - `Blank` entries are acknowledged immediately.
//! - `Membership` entries are saved to the meta store synchronously (eagerly, mid-batch)
//!   so they are not lost if the process crashes before the batch is fully applied.
//! - `Normal(ReplayRecord)` entries record their `LogId` in the `RaftApplied` column family
//!   **before** forwarding to the pipeline. This makes `applied_state()` crash-safe: if the
//!   process dies before `BlockApplier` writes to the WAL, the WAL's latest block is still N-1,
//!   so `applied_state()` returns N-1's `LogId` and OpenRaft re-applies entry N on restart.
//!

use crate::storage::{RaftStateMachineMetaStore, io_err, io_err_msg};
use openraft::storage::{RaftSnapshotBuilder, RaftStateMachine as RaftStateMachineTrait};
use openraft::{
    Entry, EntryPayload, ErrorSubject, ErrorVerb, LogId, Snapshot, SnapshotMeta, StorageError,
    StoredMembership,
};
use reth_network_peers::PeerId;
use tokio::sync::mpsc;
use zksync_os_consensus_types::{RaftNode, RaftTypeConfig, debug_display_raft_entry};
use zksync_os_rocksdb::RocksDB;
use zksync_os_storage_api::{ReadReplay, ReplayRecord};

#[derive(Debug)]
pub struct RaftStateMachineStore {
    pub(crate) meta_store: RaftStateMachineMetaStore,
    /// Unbounded to avoid deadlock during `reapply_committed()` at startup,
    /// which runs inside `Raft::new()` before the pipeline is consuming from the other end.
    pub(crate) applied_sender: mpsc::UnboundedSender<ReplayRecord>,
    /// Read-only handle to the WAL. Used by `applied_state()` to derive the last
    /// applied `LogId` from the WAL's latest committed block number.
    pub(crate) wal: Box<dyn ReadReplay>,
}

impl RaftStateMachineStore {
    /// Constructs state-machine store using raft DB handle, a WAL reference, and
    /// the apply-forwarding channel sender.
    pub fn new(
        db: RocksDB<crate::storage::RaftColumnFamily>,
        wal: Box<dyn ReadReplay>,
        applied_sender: mpsc::UnboundedSender<ReplayRecord>,
    ) -> Self {
        Self {
            meta_store: RaftStateMachineMetaStore::new(db),
            applied_sender,
            wal,
        }
    }
}

impl RaftStateMachineTrait<RaftTypeConfig> for RaftStateMachineStore {
    type SnapshotBuilder = NoopSnapshotBuilder;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<PeerId>>, StoredMembership<PeerId, RaftNode>), StorageError<PeerId>>
    {
        let meta = self.meta_store.load(ErrorSubject::StateMachine)?;
        let membership = meta
            .last_membership
            .unwrap_or_else(|| StoredMembership::new(None, Default::default()));

        // Derive `last_applied_log_id` from the WAL rather than from persisted meta.
        // `RaftApplied` records the `LogId` for each block before forwarding it to the
        // pipeline; the WAL write follows later in `BlockApplier`. By keying the lookup
        // on `wal.latest_record()`, we only advance `last_applied_log_id` once the block
        // is durably in the WAL, so a crash before the WAL write causes OpenRaft to
        // re-apply the missing entry on restart.
        let latest_wal_block = self.wal.latest_record();
        let last_applied_log_id = self.meta_store.load_block_log_id(latest_wal_block)?;

        // If `RaftApplied` has entries beyond the latest WAL block, the process exited
        // after `save_block_log_id` but before `BlockApplier` wrote those blocks to the WAL.
        // OpenRaft will re-apply them — no data is lost, but worth logging.
        self.log_pending_applied_blocks(latest_wal_block)?;

        Ok((last_applied_log_id, membership))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<()>, StorageError<PeerId>>
    where
        I: IntoIterator<Item = Entry<RaftTypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        tracing::debug!(
            "applying {} entries: {}",
            entries.len(),
            entries
                .iter()
                .map(debug_display_raft_entry)
                .collect::<Vec<_>>()
                .join(", "),
        );
        let mut responses = Vec::new();

        for entry in &entries {
            match &entry.payload {
                EntryPayload::Blank => responses.push(()),
                EntryPayload::Normal(data) => {
                    // Persist the log_id for this block BEFORE forwarding it to the pipeline.
                    // `applied_state()` uses WAL's latest block number to look up this entry;
                    // by writing first we guarantee that a crash between here and the WAL
                    // write results in re-application of this entry on restart (see
                    // `RaftStateMachineMetaStore::save_block_log_id` for the full rationale).
                    self.meta_store
                        .save_block_log_id(data.block_context.block_number, entry.log_id)?;

                    if let Err(error) = self.applied_sender.send(data.clone()) {
                        tracing::warn!("raft applied channel closed: {error}");
                        return Err(io_err(
                            &ErrorSubject::StateMachine,
                            ErrorVerb::Write,
                            &error,
                        ));
                    }
                    responses.push(());
                }
                EntryPayload::Membership(membership) => {
                    // Save membership eagerly rather than batching with other entries.
                    // If we saved it only at the end of the batch and crashed mid-batch,
                    // the membership change would be lost on restart (OpenRaft would not
                    // re-apply it because `applied_state()` returns the WAL's latest log_id
                    // which may already be past this entry).
                    let mut meta = self.meta_store.load(ErrorSubject::StateMachine)?;
                    meta.last_membership = Some(StoredMembership::new(
                        Some(entry.log_id),
                        membership.clone(),
                    ));
                    self.meta_store.save(&meta, ErrorSubject::StateMachine)?;
                    tracing::debug!("membership change persisted: log_id={:?}", entry.log_id);
                    responses.push(());
                }
            }
        }

        tracing::debug!("{} entries applied", entries.len());
        Ok(responses)
    }

    // Rest of the file only contains functions related to snapshots.
    // We don't use the openraft's snapshot feature, so implementations are stubs and can be ignored.

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        NoopSnapshotBuilder
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<<RaftTypeConfig as openraft::RaftTypeConfig>::SnapshotData>, StorageError<PeerId>>
    {
        Err(io_err_msg(
            &ErrorSubject::StateMachine,
            ErrorVerb::Read,
            "snapshotting disabled",
        ))
    }

    async fn install_snapshot(
        &mut self,
        _meta: &SnapshotMeta<PeerId, RaftNode>,
        _snapshot: Box<<RaftTypeConfig as openraft::RaftTypeConfig>::SnapshotData>,
    ) -> Result<(), StorageError<PeerId>> {
        Err(io_err_msg(
            &ErrorSubject::StateMachine,
            ErrorVerb::Write,
            "snapshotting disabled",
        ))
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<RaftTypeConfig>>, StorageError<PeerId>> {
        Ok(None)
    }
}

impl RaftStateMachineStore {
    /// Scans `RaftApplied` for blocks beyond `latest_wal_block` and logs them if any.
    /// OpenRaft will re-apply them on restart.
    #[allow(clippy::result_large_err)]
    fn log_pending_applied_blocks(
        &self,
        latest_wal_block: u64,
    ) -> Result<(), StorageError<PeerId>> {
        let mut pending = vec![];
        let mut next = latest_wal_block + 1;
        while let Some(log_id) = self.meta_store.load_block_log_id(next)? {
            pending.push((next, log_id));
            next += 1;
        }
        if !pending.is_empty() {
            tracing::info!(
                "{} block(s) in RaftApplied ahead of WAL; likely crashed before WAL write \
                — OpenRaft will re-apply the missing entries (latest_wal_block={latest_wal_block}, pending={pending:?})",
                pending.len(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
/// Snapshot builder placeholder; snapshots are intentionally disabled.
pub struct NoopSnapshotBuilder;

impl RaftSnapshotBuilder<RaftTypeConfig> for NoopSnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<RaftTypeConfig>, StorageError<PeerId>> {
        Err(io_err_msg(
            &ErrorSubject::StateMachine,
            ErrorVerb::Write,
            "snapshotting disabled",
        ))
    }
}
