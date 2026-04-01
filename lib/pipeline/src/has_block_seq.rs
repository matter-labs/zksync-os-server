/// Pipeline message types implement this so `TrackedUnboundedReceiver`
/// can automatically call `health_reporter.record_processed` on receive,
/// eliminating the boilerplate `last_block` local variable in every component.
pub trait HasBlockSeq: Send + 'static {
    /// Block number of the last block represented by this message.
    /// For block-level messages this is the block's number.
    /// For batch-level messages this is the last block in the batch.
    fn block_seq(&self) -> u64;
    /// Block timestamp in seconds, or `None` if unavailable (e.g. batch-level messages).
    fn block_timestamp(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestMsg {
        seq: u64,
        ts: u64,
    }
    impl HasBlockSeq for TestMsg {
        fn block_seq(&self) -> u64 {
            self.seq
        }
        fn block_timestamp(&self) -> Option<u64> {
            Some(self.ts)
        }
    }

    #[test]
    fn trait_returns_correct_values() {
        let msg = TestMsg { seq: 42, ts: 1000 };
        assert_eq!(msg.block_seq(), 42);
        assert_eq!(msg.block_timestamp(), Some(1000));
    }

    #[test]
    fn default_block_timestamp_is_none() {
        struct MinimalMsg;
        impl HasBlockSeq for MinimalMsg {
            fn block_seq(&self) -> u64 {
                1
            }
        }
        assert_eq!(MinimalMsg.block_timestamp(), None);
    }
}
