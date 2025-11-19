use crate::watcher::L1WatcherError;
use alloy::primitives::Address;
use alloy::rpc::types::{Log, Topic, ValueOrArray};
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

    /// Returns _all_ the addresses of contracts this processor is interested in.
    /// See [`alloy::rpc::types::Filter`] documentation for more details.
    fn contract_addresses(&self) -> ValueOrArray<Address>;

    /// Invoked each time a new log matching the filter is found.
    async fn process_raw_event(&mut self, event: Log) -> Result<(), L1WatcherError>;
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

    fn contract_addresses(&self) -> ValueOrArray<Address> {
        // A single contract per processor.
        self.contract_address().into()
    }

    async fn process_raw_event(&mut self, log: Log) -> Result<(), L1WatcherError> {
        let sol_event = T::SolEvent::decode_log(&log.inner)?.data;
        let watched_event =
            T::WatchedEvent::erased_try_from(sol_event).map_err(L1WatcherError::Convert)?;
        self.process_event(watched_event).await?;
        Ok(())
    }
}

impl<T> From<T> for Box<dyn ProcessRawEvents>
where
    T: ProcessL1Event + Send + Sync + 'static,
{
    fn from(value: T) -> Self {
        Box::new(value) as Box<dyn ProcessRawEvents>
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

    /// Returns the address of the contract this processor is interested in.
    fn contract_address(&self) -> Address;

    /// Invoked each time a new event is found.
    async fn process_event(&mut self, event: Self::WatchedEvent) -> Result<(), L1WatcherError>;
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
