use crate::ReplayRecord;
use alloy::primitives::BlockNumber;
use zk_os_forward_system::run::BlockContext;

/// Read-only view on block replay data.
///
/// Two main purposes:
/// * Sequencer's state recovery (provides all information needed to replay a block after restart).
/// * Execution environment for historical blocks (e.g., as required in `eth_call`).
pub trait ReadReplay: Send + Sync + 'static {
    /// Get block's execution context by its number.
    fn get_context(&self, block_number: BlockNumber) -> Option<BlockContext>;

    /// Get full data needed to replay a block by its number.
    fn get_replay_record(&self, block_number: BlockNumber) -> Option<ReplayRecord>;
}
