use crate::ReplayRecord;
use alloy::primitives::BlockNumber;
use futures::Stream;
use futures::stream::{BoxStream, StreamExt};
use pin_project::pin_project;
use std::task::Poll;
use std::time::Duration;
use tokio::time::{Instant, Sleep};
use zksync_os_interface::types::BlockContext;

/// Read-only view on block replay storage.
///
/// This storage serves as a source of truth about blocks executed in the past by the sequencer. No
/// block is considered canonical until it becomes a part of the replay storage. Likewise, a block
/// added to replay storage is considered to be canonical and immutable.
///
/// Node components SHOULD rely on this storage for all purposes related to historical block
/// execution. They MAY rely on other sources to expose information about mined blocks, transactions
/// and state changes as long as it is expected that the information MAY change if a different block
/// gets appended to replay storage.
///
/// All methods in this trait use RFC-2119 keywords to describe their requirements. These
/// requirements must hold for an unspecified period of time that is no less than `self`'s lifetime.
/// This is left ambiguous on purpose to allow both in-memory and persistent implementations, hence
/// any specific implementation SHOULD declare if it satisfies requirements for a longer period of
/// time.
pub trait ReadReplay: Send + Sync + 'static {
    /// Get block's execution context. Meant to be used in situations where the full block data is
    /// not needed.
    ///
    /// This method:
    /// * MUST be thread-safe
    /// * MUST return `Some(_)` if [`get_replay_record`](Self::get_replay_record) returns `Some(_)`
    ///   for the same block number; see its documentation for the full list of requirements
    fn get_context(&self, block_number: BlockNumber) -> Option<BlockContext>;

    /// Get full data needed to replay a block by its number.
    ///
    /// This method:
    /// * MUST be thread-safe
    /// * MUST return `Some(_)` for all block numbers in range `[0; latest_record()]`
    /// * MUST return the same value for any block number once it returns `Some(_)` at least once
    /// * MAY return `Some(_)` for block numbers after latest
    fn get_replay_record(&self, block_number: BlockNumber) -> Option<ReplayRecord>;

    /// Returns the latest (greatest) record's block number.
    ///
    /// This method:
    /// * MUST be thread-safe
    /// * MUST be infallible, as replay storage is guaranteed to hold at least genesis under `0`
    /// * MUST be monotonically non-decreasing
    ///
    /// If this method returned `N`, then **all** replay records in range `[0; N]` MUST be available
    /// in storage. "Available" here means that they can be fetched by
    /// [`get_replay_record`](Self::get_replay_record) or [`get_context`](Self::get_context), both of
    /// which MUST return `Some(_)`.
    fn latest_record(&self) -> BlockNumber;
}

/// Extension methods for [`ReadReplay`].
pub trait ReadReplayExt: ReadReplay {
    /// Streams all replay records with block_number ≥ `start`, in ascending block order. Finishes
    /// after reaching the latest stored record. Used to replay all blocks when recovering state.
    fn stream_from(&self, start: u64) -> BoxStream<ReplayRecord> {
        let latest = self.latest_record();
        let stream = futures::stream::iter(start..=latest).filter_map(move |block_num| {
            let record = self.get_replay_record(block_num);
            match record {
                Some(record) => futures::future::ready(Some(record)),
                None => futures::future::ready(None),
            }
        });
        Box::pin(stream)
    }

    /// Streams all replay records with block_number ≥ `start`, in ascending block order. On reaching
    /// the latest stored record continuously waits for new records to appear. Used to send blocks to ENs.
    fn stream_from_forever(&self, start: BlockNumber) -> BoxStream<ReplayRecord>
    where
        Self: Clone,
    {
        #[pin_project]
        struct BlockStream<Replay: ReadReplay> {
            replays: Replay,
            current_block: BlockNumber,
            #[pin]
            sleep: Sleep,
        }
        impl<Replay: ReadReplay> Stream for BlockStream<Replay> {
            type Item = ReplayRecord;

            fn poll_next(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                let mut this = self.project();
                if let Some(record) = this.replays.get_replay_record(*this.current_block) {
                    *this.current_block += 1;
                    Poll::Ready(Some(record))
                } else {
                    // TODO: would be nice to be woken up only when the next block is available
                    this.sleep
                        .as_mut()
                        .reset(Instant::now() + Duration::from_millis(50));
                    assert_eq!(this.sleep.poll(cx), Poll::Pending);
                    Poll::Pending
                }
            }
        }

        Box::pin(BlockStream {
            replays: self.clone(),
            current_block: start,
            sleep: tokio::time::sleep(Duration::from_millis(50)),
        })
    }
}

impl<T: ReadReplay> ReadReplayExt for T {}

/// A write-capable counterpart of [`ReadReplay`] that allows to append new records to the storage.
///
/// This trait is meant to be solely-owned by sequencer and to append replay records synchronously one
/// by one. Thus, thread-safety is optional.
///
/// Implementation MUST guarantee that [`append`](Self::append) is the only way to mutate state
/// inside storage. Trait's consumer MAY depend on state being immutable while they do not call `append`.
pub trait WriteReplay: ReadReplay {
    /// Appends a new record to replay storage. Returns `true` when a new `RelayRecord` was appended
    /// - `false` otherwise.
    ///
    /// This method:
    /// * MAY be thread-safe
    /// * MUST return `false` when inserting a record with an existing block number, storage must
    ///   remain unchanged
    /// * MUST panic if the record is not next after the latest record (as returned by [`latest_record`](Self::latest_record))
    /// * MUST return `true` when the record was successfully added to storage, at which point
    ///   all [`ReadReplay`] methods should reflect its existence appropriately
    /// * MUST be atomic and always leave storage in a valid state (that satisfies all requirements
    ///   here and in [`ReadReplay`]) regardless of the method's outcome (including panic)
    fn append(&self, record: ReplayRecord) -> bool;
}
