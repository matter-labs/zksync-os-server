use crate::watcher::L1WatcherError;
use alloy::primitives::B256;
use alloy::providers::DynProvider;
use alloy::rpc::types::{Log, Topic};
use alloy::sol_types::SolEvent;

/// A "raw" event processor that works with decoded logs.
/// Provides more flexibility compared to [`ProcessL1Event`], but requires the author
/// to handle log decoding manually.
///
/// This trait is what is actually used by [`L1Watcher`], although any type that implements
/// [`ProcessL1Event`] implements it automatically.
///
/// For simple use cases where you need to process a single type of event from a single contract,
/// prefer implementing [`ProcessL1Event`] instead.
///
/// This type is object-safe and can be used as a trait object.
#[async_trait::async_trait]
pub trait ProcessRawEvents: Send + Sync + 'static {
    /// The name of this processor, used for metrics and logging.
    fn name(&self) -> &'static str;

    /// Returns the combined `Topic` for _all_ the event signatures this processor is interested in.
    /// See [`alloy::rpc::types::Filter`] documentation for more details.
    fn event_signatures(&self) -> Topic;

    fn filter_events(&self, logs: Vec<Log>) -> Vec<Log>;

    /// Optional filter on topic1 (the first indexed event parameter) to include in the
    /// `eth_getLogs` query. When `Some`, the RPC node filters logs server-side.
    fn topic1_filter(&self) -> Option<B256> {
        None
    }

    /// Invoked each time a new log matching the filter is found.
    ///
    /// `provider` is the settlement-layer provider the [`L1Watcher`](crate::L1Watcher) used to
    /// fetch the log. SL-aware processors that follow batches across L1 ↔ Gateway boundaries can
    /// use it to fetch additional data (e.g. commit calldata) against the right SL without
    /// storing a stale provider reference; single-SL processors can ignore it.
    async fn process_raw_event(
        &mut self,
        provider: &DynProvider,
        event: Log,
    ) -> Result<(), L1WatcherError>;
}

/// Blanket implementation of `ProcessRawEvents` for any type implementing `ProcessL1Event`.
#[async_trait::async_trait]
impl<T> ProcessRawEvents for T
where
    T: ProcessL1Event + Send + Sync + 'static,
{
    fn name(&self) -> &'static str {
        T::NAME
    }

    fn event_signatures(&self) -> Topic {
        // A single event per processor.
        T::SolEvent::SIGNATURE_HASH.into()
    }

    fn filter_events(&self, logs: Vec<Log>) -> Vec<Log> {
        logs
    }

    fn topic1_filter(&self) -> Option<B256> {
        ProcessL1Event::topic1_filter(self)
    }

    async fn process_raw_event(
        &mut self,
        provider: &DynProvider,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        let sol_event = T::SolEvent::decode_log(&log.inner)?.data;
        let watched_event =
            T::WatchedEvent::erased_try_from(sol_event).map_err(L1WatcherError::Convert)?;
        self.process_event(provider, watched_event, log).await?;
        Ok(())
    }
}

/// A typesafe implementation of an L1 event processor.
/// Defines a single contract and single event type to process,
/// and expects the event to be already decoded.
#[async_trait::async_trait]
pub trait ProcessL1Event {
    const NAME: &'static str;

    /// What kind of Solidity event this processor looks for.
    type SolEvent: SolEvent + Send + Sync + 'static;
    /// What do we want to process; must be convertible from `SolEvent`.
    type WatchedEvent: ErasedTryFrom<Self::SolEvent> + Send + Sync + 'static;

    /// Optional filter on topic1 (the first indexed event parameter). When `Some`, only logs
    /// where topic1 equals the given value are forwarded to [`Self::process_event`].
    fn topic1_filter(&self) -> Option<B256> {
        None
    }

    /// Invoked each time a new event is found.
    ///
    /// `provider` is the settlement-layer provider that produced the log; see
    /// [`ProcessRawEvents::process_raw_event`].
    async fn process_event(
        &mut self,
        provider: &DynProvider,
        event: Self::WatchedEvent,
        log: Log,
    ) -> Result<(), L1WatcherError>;
}

/// Implementation of `TryFrom` that erases the error type to `anyhow::Error`.
/// This is useful to make `L1WatcherError` not depend on the specific error types of each
/// processor.
pub trait ErasedTryFrom<T>: Sized {
    /// `TryFrom::try_from`, but with erased error type.
    fn erased_try_from(value: T) -> Result<Self, anyhow::Error>;
}

impl<T, U, E> ErasedTryFrom<T> for U
where
    U: TryFrom<T, Error = E>,
    E: Into<anyhow::Error>,
{
    fn erased_try_from(value: T) -> Result<Self, anyhow::Error> {
        U::try_from(value).map_err(Into::into)
    }
}
