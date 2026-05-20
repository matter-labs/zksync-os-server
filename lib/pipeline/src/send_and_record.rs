use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::has_block_range_end::HasBlockRangeEnd;
use crate::metrics::PIPELINE_METRICS;

/// If a blocked send does not complete within this duration the downstream
/// consumer is considered stuck and the component shuts down with an error.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Error returned by [`SendAndRecordExt::send_and_record`].
pub enum PipelineSendError<T> {
    /// The channel's receiver was dropped.
    Closed(T),
    /// The channel was full and the downstream consumer did not drain it
    /// within [`STALL_TIMEOUT`]. The consumer is considered stuck.
    Stuck,
}

impl<T> fmt::Debug for PipelineSendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed(_) => write!(f, "PipelineSendError::Closed(..)"),
            Self::Stuck => write!(f, "PipelineSendError::Stuck"),
        }
    }
}

impl<T> fmt::Display for PipelineSendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed(_) => write!(
                f,
                "pipeline channel closed: downstream receiver was dropped"
            ),
            Self::Stuck => write!(
                f,
                "pipeline channel stuck: downstream consumer did not drain within {STALL_TIMEOUT:?}"
            ),
        }
    }
}

impl<T> std::error::Error for PipelineSendError<T> {}

/// Extension trait on `mpsc::Sender<T>` that combines sending an item
/// with recording it as processed on a `ComponentStateReporter`.
///
/// Uses `try_send` as a zero-cost fast path. If the channel is full, logs
/// and increments the stall counter, then blocks via `send().await`
/// until space is available. If the consumer does not drain the channel within
/// [`STALL_TIMEOUT`], logs an `error` (forwarded to Sentry) and returns
/// [`PipelineSendError::Stuck`] so the component shuts down cleanly.
/// Recording happens only after a successful send.
#[async_trait]
pub trait SendAndRecordExt<T: HasBlockRangeEnd> {
    async fn send_and_record(
        &self,
        value: T,
        reporter: &zksync_os_observability::ComponentStateReporter,
    ) -> Result<(), PipelineSendError<T>>;
}

#[async_trait]
impl<T: HasBlockRangeEnd> SendAndRecordExt<T> for mpsc::Sender<T> {
    async fn send_and_record(
        &self,
        value: T,
        reporter: &zksync_os_observability::ComponentStateReporter,
    ) -> Result<(), PipelineSendError<T>> {
        let block_number = value.block_number();
        let block_timestamp = value.block_timestamp();
        let batch_number = value.batch_number();

        match self.try_send(value) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(v)) => return Err(PipelineSendError::Closed(v)),
            Err(mpsc::error::TrySendError::Full(value)) => {
                let component = reporter.component_name();
                tracing::info!(
                    component,
                    "pipeline channel stall: output channel is full - downstream consumer is behind"
                );
                PIPELINE_METRICS.channel_stall_count[&component].inc();
                match tokio::time::timeout(STALL_TIMEOUT, self.send(value)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(mpsc::error::SendError(v))) => return Err(PipelineSendError::Closed(v)),
                    Err(_elapsed) => {
                        tracing::error!(
                            component,
                            "pipeline channel stuck: downstream consumer did not pick up within \
                             {STALL_TIMEOUT:?}, shutting down"
                        );
                        return Err(PipelineSendError::Stuck);
                    }
                }
            }
        }

        reporter.record_processed(block_number, block_timestamp, batch_number);
        Ok(())
    }
}
