/// Whether the node should be accepting transactions
#[derive(Debug, Clone)]
pub enum TransactionAcceptanceState {
    Accepting,
    NotAccepting(NotAcceptingReason),
}

/// Reason why the node is not accepting transactions
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum NotAcceptingReason {
    /// Block production has been disabled via config (`sequencer_max_blocks_to_produce`)
    #[error("Node is not currently accepting transactions: block production disabled.")]
    BlockProductionDisabled,
    #[error("Transaction submission not implemented on external nodes.")]
    ExternalNode,
}
