/// Pipeline message types implement this so `PeekableReceiver::recv_and_record_picked`
/// and `SendAndRecordExt::send_and_record` can pull block/batch coordinates off the
/// message and drive `ComponentStateReporter` automatically, eliminating the
/// boilerplate `last_block` local variable in every component.
pub trait HasBlockRangeEnd: Send + 'static {
    /// Block number of the last block represented by this message.
    /// For batch-level messages this is the last block in the batch.
    fn block_number(&self) -> u64;
    /// Block timestamp in seconds, or `None` if unavailable (e.g. batch-level messages).
    fn block_timestamp(&self) -> Option<u64> {
        None
    }
    /// Batch number of the last batch represented by this message, if applicable.
    fn batch_number(&self) -> Option<u64> {
        None
    }
}
