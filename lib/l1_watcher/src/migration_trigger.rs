/// Signal published by `MigrationGate` when it detects a `SetSLChainId` system transaction in a
/// commit batch flowing through the L1 sender pipeline. Consumers use it to coordinate the
/// settlement-layer migration handoff.
///
/// Carries both the batch number (used by [`SettlementLayerWatcher`][crate::SettlementLayerWatcher]
/// to know when preceding batches have been executed on L1) and the block number containing the
/// `SetSLChainId` transaction (used by the `zks_lastSettlementChangeBlock` RPC). The block number
/// is needed because in-flight batches have not yet been persisted in local batch storage at the
/// moment the signal fires, so the block number cannot be recovered from there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationTrigger {
    /// Batch number of the commit batch that contains the `SetSLChainId` system transaction.
    pub batch_number: u64,
    /// Block number containing the `SetSLChainId` system transaction (always the first block of
    /// the trigger batch — the transaction is always the first one in the first block of the
    /// first batch in a new settlement-layer interval).
    pub block_number: u64,
}
