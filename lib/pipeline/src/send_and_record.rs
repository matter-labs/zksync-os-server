use crate::has_block_range_end::HasBlockRangeEnd;
use tokio::sync::mpsc;

/// Extension trait on `mpsc::UnboundedSender<T>` that combines sending an item
/// with recording it as processed on a `ComponentStateReporter`.
///
/// Recording happens only if the send succeeds — if the receiver has been
/// dropped, the error is returned and nothing is recorded.
pub trait SendAndRecordExt<T: HasBlockRangeEnd> {
    fn send_and_record(
        &self,
        value: T,
        reporter: &zksync_os_observability::ComponentStateReporter,
    ) -> Result<(), mpsc::error::SendError<T>>;
}

impl<T: HasBlockRangeEnd> SendAndRecordExt<T> for mpsc::UnboundedSender<T> {
    fn send_and_record(
        &self,
        value: T,
        reporter: &zksync_os_observability::ComponentStateReporter,
    ) -> Result<(), mpsc::error::SendError<T>> {
        let block_number = value.block_number();
        let block_timestamp = value.block_timestamp();
        let batch_number = value.batch_number();
        // Send first so we only record successful hand-offs. The downstream consumer
        // may record its picked/processed watermark before we record ours, producing a
        // transient negative adjacent lag — the gauge saturates at 0.
        self.send(value)?;
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
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        let (reporter, state_rx) = ComponentStateReporter::new("test");

        assert!(
            tx.send_and_record(Msg { seq: 7, ts: 700 }, &reporter)
                .is_ok()
        );
        assert_eq!(
            state_rx
                .borrow()
                .last_processed
                .as_ref()
                .map(|c| c.block_number),
            Some(7)
        );
        assert_eq!(
            state_rx
                .borrow()
                .last_processed
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

        let (tx, rx) = mpsc::unbounded_channel::<MsgNoTs>();
        drop(rx);
        let (reporter, state_rx) = ComponentStateReporter::new("test");

        assert!(tx.send_and_record(MsgNoTs { seq: 1 }, &reporter).is_err());
        assert!(state_rx.borrow().last_processed.is_none());
    }
}
