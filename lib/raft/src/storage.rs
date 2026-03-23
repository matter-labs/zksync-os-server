//! Raft log storage low-level primitives backed by RocksDB.
//!
//! This module implements log-side OpenRaft storage (`RaftLogStorage` / `RaftLogReader`).
//! It also owns low-level state-machine metadata persistence primitives that are
//! consumed by `state_machine.rs`.

use openraft::storage::{LogFlushed, LogState, RaftLogReader, RaftLogStorage};
use openraft::{
    AnyError, Entry, ErrorSubject, ErrorVerb, LogId, StorageError, StorageIOError,
    StoredMembership, Vote,
};
use reth_network_peers::PeerId;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::future::Future;
use std::ops::RangeBounds;
use std::path::Path;
use zksync_os_consensus_types::{RaftNode, RaftTypeConfig};
use zksync_os_rocksdb::db::{NamedColumnFamily, WriteBatch};
use zksync_os_rocksdb::RocksDB;

#[derive(Clone, Debug)]
pub struct RaftLogStore {
    db: RocksDB<RaftColumnFamily>,
}

#[derive(Clone, Debug)]
pub(crate) struct RaftStateMachineMetaStore {
    db: RocksDB<RaftColumnFamily>,
}

#[derive(Copy, Clone, Debug)]
pub enum RaftColumnFamily {
    /// Raft log entries.
    Logs,
    /// Persisted vote.
    Vote,
    /// Log metadata (`committed`).
    LogMeta,
    /// State-machine metadata (last membership).
    StateMachineMeta,
    /// Maps WAL block number (u64 BE) → serialized `LogId<PeerId>`.
    ///
    /// Written by `apply()` **before** forwarding a block to the execution pipeline.
    /// This makes `applied_state()` safe to derive from the WAL: if the process crashes
    /// between the channel send and the WAL write, the WAL's latest block is still N-1,
    /// so `applied_state()` returns N-1's `LogId` and OpenRaft correctly re-applies entry N.
    RaftApplied,
}

impl NamedColumnFamily for RaftColumnFamily {
    const DB_NAME: &'static str = "raft";
    const ALL: &'static [Self] = &[
        RaftColumnFamily::Logs,
        RaftColumnFamily::Vote,
        RaftColumnFamily::LogMeta,
        RaftColumnFamily::StateMachineMeta,
        RaftColumnFamily::RaftApplied,
    ];

    fn name(&self) -> &'static str {
        match self {
            RaftColumnFamily::Logs => "logs",
            RaftColumnFamily::Vote => "vote",
            RaftColumnFamily::LogMeta => "log_meta",
            RaftColumnFamily::StateMachineMeta => "state_machine_meta",
            RaftColumnFamily::RaftApplied => "raft_applied",
        }
    }
}

pub(crate) fn io_err<E: std::error::Error + 'static>(
    subject: &ErrorSubject<PeerId>,
    verb: ErrorVerb,
    err: &E,
) -> StorageError<PeerId> {
    StorageError::IO {
        source: StorageIOError::new(subject.clone(), verb, AnyError::new(err)),
    }
}

pub(crate) fn io_err_msg(
    subject: &ErrorSubject<PeerId>,
    verb: ErrorVerb,
    msg: impl ToString,
) -> StorageError<PeerId> {
    StorageError::IO {
        source: StorageIOError::new(subject.clone(), verb, AnyError::error(msg)),
    }
}

impl RaftLogStore {
    /// Opens raft storage DB with sync writes enabled.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let db = RocksDB::<RaftColumnFamily>::new(path)
            .map_err(|e| anyhow::anyhow!("opening raft db at {}: {e}", path.display()))?
            .with_sync_writes();
        Ok(Self { db })
    }

    /// Returns a clone of the underlying raft RocksDB handle.
    pub(crate) fn db(&self) -> RocksDB<RaftColumnFamily> {
        self.db.clone()
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct RaftStateMachineMeta {
    pub(crate) last_membership: Option<StoredMembership<PeerId, RaftNode>>,
}

impl RaftStateMachineMetaStore {
    const STATE_MACHINE_META_KEY: &'static [u8] = b"state_machine_meta";

    pub(crate) fn new(db: RocksDB<RaftColumnFamily>) -> Self {
        Self { db }
    }

    pub(crate) fn load(
        &self,
        subject: ErrorSubject<PeerId>,
    ) -> Result<RaftStateMachineMeta, StorageError<PeerId>> {
        let bytes = self
            .db
            .get_cf(RaftColumnFamily::StateMachineMeta, Self::STATE_MACHINE_META_KEY)
            .map_err(|e| io_err(&subject, ErrorVerb::Read, &e))?;
        let Some(bytes) = bytes else {
            return Ok(RaftStateMachineMeta::default());
        };
        let meta = bincode::serde::decode_from_slice::<RaftStateMachineMeta, _>(
            &bytes,
            bincode::config::standard(),
        )
        .map_err(|e| io_err(&subject, ErrorVerb::Read, &e))?
        .0;
        Ok(meta)
    }

    /// Persists the `LogId` for a block that has been applied to the state machine.
    ///
    /// Must be called **before** forwarding the block to the execution pipeline channel.
    /// This ordering guarantees crash safety: if the process dies before the WAL write,
    /// `applied_state()` reads the WAL's previous block number and finds its (already
    /// persisted) `LogId`, causing OpenRaft to re-apply the lost entry on the next startup.
    pub(crate) fn save_block_log_id(
        &self,
        block_number: u64,
        log_id: LogId<PeerId>,
    ) -> Result<(), StorageError<PeerId>> {
        let key = block_number.to_be_bytes();
        let encoded = bincode::serde::encode_to_vec(log_id, bincode::config::standard())
            .expect("serialize log id");
        let mut batch = self.db.new_write_batch();
        batch.put_cf(RaftColumnFamily::RaftApplied, &key, &encoded);
        self.db
            .write(batch)
            .map_err(|e| io_err(&ErrorSubject::StateMachine, ErrorVerb::Write, &e))?;
        Ok(())
    }

    /// Reads back the `LogId` that was saved for a given WAL block number,
    /// returning `None` if no entry exists (e.g. genesis).
    pub(crate) fn load_block_log_id(
        &self,
        block_number: u64,
    ) -> Result<Option<LogId<PeerId>>, StorageError<PeerId>> {
        let key = block_number.to_be_bytes();
        let bytes = self
            .db
            .get_cf(RaftColumnFamily::RaftApplied, &key)
            .map_err(|e| io_err(&ErrorSubject::StateMachine, ErrorVerb::Read, &e))?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let log_id = bincode::serde::decode_from_slice::<LogId<PeerId>, _>(
            &bytes,
            bincode::config::standard(),
        )
        .map_err(|e| io_err(&ErrorSubject::StateMachine, ErrorVerb::Read, &e))?
        .0;
        Ok(Some(log_id))
    }

    pub(crate) fn save(
        &self,
        meta: &RaftStateMachineMeta,
        subject: ErrorSubject<PeerId>,
    ) -> Result<(), StorageError<PeerId>> {
        let encoded = bincode::serde::encode_to_vec(meta, bincode::config::standard())
            .expect("serialize raft state machine meta");
        let mut batch: WriteBatch<'_, RaftColumnFamily> = self.db.new_write_batch();
        batch.put_cf(
            RaftColumnFamily::StateMachineMeta,
            Self::STATE_MACHINE_META_KEY,
            &encoded,
        );
        self.db
            .write(batch)
            .map_err(|e| io_err(&subject, ErrorVerb::Write, &e))?;
        Ok(())
    }
}

impl RaftLogStore {
    const COMMITTED_KEY: &'static [u8] = b"committed";
    const VOTE_KEY: &'static [u8] = b"vote";

    fn index_key(index: u64) -> [u8; 8] {
        index.to_be_bytes()
    }

    fn decode_entry(bytes: &[u8]) -> Result<Entry<RaftTypeConfig>, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    fn encode_entry(entry: &Entry<RaftTypeConfig>) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(entry)
    }

    fn last_log_id_from_db(&self) -> Result<Option<LogId<PeerId>>, StorageError<PeerId>> {
        let end = u64::MAX.to_be_bytes();
        let mut iter = self
            .db
            .to_iterator_cf(RaftColumnFamily::Logs, ..=end.as_slice());
        if let Some((_key, value)) = iter.next() {
            let entry = Self::decode_entry(&value)
                .map_err(|e| io_err(&ErrorSubject::Store, ErrorVerb::Read, &e))?;
            Ok(Some(entry.log_id))
        } else {
            Ok(None)
        }
    }
}

impl RaftLogReader<RaftTypeConfig> for RaftLogStore {
    fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> impl Future<Output = Result<Vec<Entry<RaftTypeConfig>>, StorageError<PeerId>>> + Send {
        async move {
            let start = match range.start_bound() {
                std::ops::Bound::Included(v) => *v,
                std::ops::Bound::Excluded(v) => v.saturating_add(1),
                std::ops::Bound::Unbounded => 0,
            };
            let end = match range.end_bound() {
                std::ops::Bound::Included(v) => Some(*v),
                std::ops::Bound::Excluded(v) => v.checked_sub(1),
                std::ops::Bound::Unbounded => None,
            };

            let mut entries = Vec::new();
            let start_key = Self::index_key(start);
            for (key, value) in self
                .db
                .from_iterator_cf(RaftColumnFamily::Logs, start_key.as_slice()..)
            {
                let index = u64::from_be_bytes(
                    key.as_ref().try_into().map_err(|_| {
                        io_err_msg(&ErrorSubject::Store, ErrorVerb::Read, "invalid raft log index key")
                    })?,
                );
                if let Some(end) = end {
                    if index > end {
                        break;
                    }
                }
                let entry = Self::decode_entry(&value)
                    .map_err(|e| io_err(&ErrorSubject::Store, ErrorVerb::Read, &e))?;
                entries.push(entry);
            }
            Ok(entries)
        }
    }
}

impl RaftLogStorage<RaftTypeConfig> for RaftLogStore {
    type LogReader = RaftLogStore;

    fn get_log_state(&mut self) -> impl Future<Output = Result<LogState<RaftTypeConfig>, StorageError<PeerId>>> + Send {
        async move {
            let last_log_id = self.last_log_id_from_db()?;
            Ok(LogState {
                // Purging/snapshotting is intentionally disabled.
                last_purged_log_id: None,
                last_log_id,
            })
        }
    }

    fn get_log_reader(&mut self) -> impl Future<Output = Self::LogReader> + Send {
        async move { self.clone() }
    }

    fn save_vote(&mut self, vote: &Vote<PeerId>) -> impl Future<Output = Result<(), StorageError<PeerId>>> + Send {
        async move {
            let bytes = bincode::serde::encode_to_vec(vote, bincode::config::standard())
                .expect("serialize vote");
            let mut batch = self.db.new_write_batch();
            batch.put_cf(RaftColumnFamily::Vote, Self::VOTE_KEY, &bytes);
            self.db
                .write(batch)
                .map_err(|e| io_err(&ErrorSubject::Store, ErrorVerb::Write, &e))?;
            Ok(())
        }
    }

    fn read_vote(&mut self) -> impl Future<Output = Result<Option<Vote<PeerId>>, StorageError<PeerId>>> + Send {
        async move {
            let bytes = self
                .db
                .get_cf(RaftColumnFamily::Vote, Self::VOTE_KEY)
                .map_err(|e| io_err(&ErrorSubject::Store, ErrorVerb::Read, &e))?;
            let Some(bytes) = bytes else {
                return Ok(None);
            };
            let vote = bincode::serde::decode_from_slice::<Vote<PeerId>, _>(&bytes, bincode::config::standard())
                .map_err(|e| io_err(&ErrorSubject::Store, ErrorVerb::Read, &e))?
                .0;
            Ok(Some(vote))
        }
    }

    fn save_committed(
        &mut self,
        committed: Option<LogId<PeerId>>,
    ) -> impl Future<Output = Result<(), StorageError<PeerId>>> + Send {
        async move {
            let mut batch = self.db.new_write_batch();
            if let Some(committed) = committed {
                let encoded = bincode::serde::encode_to_vec(committed, bincode::config::standard())
                    .expect("serialize committed log id");
                batch.put_cf(RaftColumnFamily::LogMeta, Self::COMMITTED_KEY, &encoded);
            } else {
                batch.delete_cf(RaftColumnFamily::LogMeta, Self::COMMITTED_KEY);
            }
            self.db
                .write(batch)
                .map_err(|e| io_err(&ErrorSubject::Store, ErrorVerb::Write, &e))?;
            Ok(())
        }
    }

    fn read_committed(
        &mut self,
    ) -> impl Future<Output = Result<Option<LogId<PeerId>>, StorageError<PeerId>>> + Send {
        async move {
            let bytes = self
                .db
                .get_cf(RaftColumnFamily::LogMeta, Self::COMMITTED_KEY)
                .map_err(|e| io_err(&ErrorSubject::Store, ErrorVerb::Read, &e))?;
            let Some(bytes) = bytes else {
                return Ok(None);
            };
            let committed =
                bincode::serde::decode_from_slice::<LogId<PeerId>, _>(&bytes, bincode::config::standard())
                    .map_err(|e| io_err(&ErrorSubject::Store, ErrorVerb::Read, &e))?
                    .0;
            Ok(Some(committed))
        }
    }

    fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<RaftTypeConfig>,
    ) -> impl Future<Output = Result<(), StorageError<PeerId>>> + Send
    where
        I: IntoIterator<Item = Entry<RaftTypeConfig>> + Send,
        I::IntoIter: Send,
    {
        async move {
            let mut batch = self.db.new_write_batch();
            for entry in entries {
                let key = Self::index_key(entry.log_id.index);
                let value = Self::encode_entry(&entry)
                    .map_err(|e| io_err(&ErrorSubject::Store, ErrorVerb::Write, &e))?;
                batch.put_cf(RaftColumnFamily::Logs, &key, &value);
            }
            self.db
                .write(batch)
                .map_err(|e| io_err(&ErrorSubject::Store, ErrorVerb::Write, &e))?;
            callback.log_io_completed(Ok(()));
            Ok(())
        }
    }

    fn truncate(&mut self, log_id: LogId<PeerId>) -> impl Future<Output = Result<(), StorageError<PeerId>>> + Send {
        async move {
            let start_key = Self::index_key(log_id.index);
            let mut batch = self.db.new_write_batch();
            for (key, _value) in self
                .db
                .from_iterator_cf(RaftColumnFamily::Logs, start_key.as_slice()..)
            {
                batch.delete_cf(RaftColumnFamily::Logs, &key);
            }
            self.db
                .write(batch)
                .map_err(|e| io_err(&ErrorSubject::Store, ErrorVerb::Write, &e))?;
            Ok(())
        }
    }

    fn purge(&mut self, log_id: LogId<PeerId>) -> impl Future<Output = Result<(), StorageError<PeerId>>> + Send {
        async move {
            let mut batch = self.db.new_write_batch();
            let start = Self::index_key(0);
            let end = Self::index_key(log_id.index.saturating_add(1));
            batch.delete_range_cf(RaftColumnFamily::Logs, start.as_slice()..end.as_slice());
            self.db
                .write(batch)
                .map_err(|e| io_err(&ErrorSubject::Store, ErrorVerb::Write, &e))?;
            Ok(())
        }
    }
}
