/// Pipeline message types implement this so `TrackedUnboundedReceiver`
/// can automatically call `health_reporter.record_processed` on receive,
/// eliminating the boilerplate `last_block` local variable in every component.
pub trait HasBlockRangeEnd: Send + 'static {
    /// Block number of the last block represented by this message.
    /// For block-level messages this is the block's number.
    /// For batch-level messages this is the last block in the batch.
    fn block_number(&self) -> u64;
    /// Block timestamp in seconds, or `None` if unavailable (e.g. batch-level messages).
    fn block_timestamp(&self) -> Option<u64> {
        None
    }
}
