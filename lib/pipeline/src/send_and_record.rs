use crate::has_block_range_end::HasBlockRangeEnd;
use tokio::sync::mpsc;

/// Extension trait on `mpsc::Sender<T>` that combines sending an item
/// with recording it as processed on a `ComponentStateReporter`.
///
/// Recording happens only if the send succeeds — if the receiver has been
/// dropped, the error is returned and nothing is recorded. If the channel is
/// full, this method panics; it assumes a large-capacity channel
/// (`OUTPUT_CHANNEL_CAPACITY`) that should never fill under normal operation.
pub trait SendAndRecordExt<T: HasBlockRangeEnd> {
    fn send_and_record(
        &self,
        value: T,
        reporter: &zksync_os_observability::ComponentStateReporter,
    ) -> Result<(), mpsc::error::SendError<T>>;
}

impl<T: HasBlockRangeEnd> SendAndRecordExt<T> for mpsc::Sender<T> {
    fn send_and_record(
        &self,
        value: T,
        reporter: &zksync_os_observability::ComponentStateReporter,
    ) -> Result<(), mpsc::error::SendError<T>> {
        let block_number = value.block_number();
        let block_timestamp = value.block_timestamp();
        let batch_number = value.batch_number();
        match self.try_send(value) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(v)) => return Err(mpsc::error::SendError(v)),
            Err(mpsc::error::TrySendError::Full(_)) => {
                panic!("pipeline channel unexpectedly full — consumer is catastrophically behind")
            }
        }
        reporter.record_processed(block_number, block_timestamp, batch_number);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zksync_os_observability::ComponentStateReporter;

    struct Msg {
        seq: u64,
        ts: u64,
    }
    impl HasBlockRangeEnd for Msg {
        fn block_number(&self) -> u64 {
            self.seq
        }
        fn block_timestamp(&self) -> Option<u64> {
            Some(self.ts)
        }
    }

    #[tokio::test]
    async fn records_on_success() {
        let (tx, mut rx) = mpsc::channel::<Msg>(16);
        let (reporter, state_rx) = ComponentStateReporter::new("test");

        assert!(
            tx.send_and_record(Msg { seq: 7, ts: 700 }, &reporter)
                .is_ok()
        );
        assert_eq!(
            state_rx
                .borrow()
                .block_processed
                .as_ref()
                .map(|c| c.block_number),
            Some(7)
        );
        assert_eq!(
            state_rx
                .borrow()
                .block_processed
                .as_ref()
                .and_then(|c| c.timestamp),
            Some(700)
        );
        assert_eq!(rx.recv().await.unwrap().seq, 7);
    }

    #[tokio::test]
    async fn does_not_record_when_receiver_dropped() {
        struct MsgNoTs {
            seq: u64,
        }
        impl HasBlockRangeEnd for MsgNoTs {
            fn block_number(&self) -> u64 {
                self.seq
            }
        }

        let (tx, rx) = mpsc::channel::<MsgNoTs>(1);
        drop(rx);
        let (reporter, state_rx) = ComponentStateReporter::new("test");

        assert!(tx.send_and_record(MsgNoTs { seq: 1 }, &reporter).is_err());
        assert!(state_rx.borrow().block_processed.is_none());
    }
}
