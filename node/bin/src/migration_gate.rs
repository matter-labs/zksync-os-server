use tokio::sync::mpsc;
use tokio::sync::watch;
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{ComponentId, PeekableReceiver, PipelineComponent, SendAndRecordExt};

/// A pipeline component that acts as a gate in front of the L1 commit sender.
///
/// Under normal operation it is transparent — items flow straight through.
///
/// The gate activates when it observes a `SendToL1` commit batch whose
/// `set_sl_chain_id_migration_number` is greater than the last-finalized migration counter
/// maintained by [`MigrationFinalizedWatcher`][zksync_os_l1_watcher::MigrationFinalizedWatcher].
/// In that case it:
/// 1. Signals `migration_triggered` with the batch number so that
///    [`SettlementLayerWatcher`][zksync_os_l1_watcher::SettlementLayerWatcher] can check
///    whether all preceding batches have been executed before crashing the node.
/// 2. Pauses all subsequent batches until the counter reaches the batch's migration number.
pub struct MigrationGate {
    /// Last-finalized migration number on the current SL. Initialized at startup from
    /// `IChainAssetHandler.migrationNumber(chainId)` and updated only by
    /// [`MigrationFinalizedWatcher`][zksync_os_l1_watcher::MigrationFinalizedWatcher].
    pub last_finalized_migration: watch::Receiver<u64>,
    /// Notifies `SettlementLayerWatcher` of the batch number that contains `SetSLChainId`.
    /// Sent as soon as the triggering batch is detected, before entering the wait.
    pub migration_triggered: watch::Sender<Option<u64>>,
}

#[async_trait::async_trait]
impl PipelineComponent for MigrationGate {
    type Input = L1SenderCommand<CommitCommand>;
    type Output = L1SenderCommand<CommitCommand>;

    const COMPONENT_ID: ComponentId = ComponentId::MigrationGate;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        loop {
            state_reporter.enter_state(GenericComponentState::Idle);

            let Some(item) = input.recv_and_record_picked(&state_reporter).await else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };

            state_reporter.enter_state(GenericComponentState::Active);

            // Only `SendToL1` batches go through the gate; already-committed `Passthrough`
            // batches are forwarded unconditionally.
            let pending_migration_number = if let L1SenderCommand::SendToL1(command) = &item {
                // CommitCommand always contains exactly one envelope; use AsRef to access it.
                command
                    .as_ref()
                    .first()
                    .and_then(|e| e.batch.set_sl_chain_id_migration_number)
                    .filter(|&n| n > *self.last_finalized_migration.borrow())
            } else {
                None
            };

            if let Some(migration_number) = pending_migration_number {
                let trigger_batch_number = item.first_batch_number();
                tracing::info!(
                    migration_number,
                    trigger_batch_number,
                    "SetSLChainId batch detected; signalling settlement layer watcher and pausing commit pipeline"
                );
                // Signal before waiting so SettlementLayerWatcher can immediately start checking
                // the executed-batch precondition.
                let _ = self.migration_triggered.send(Some(trigger_batch_number));

                self.last_finalized_migration
                    .wait_for(|n| *n >= migration_number)
                    .await?;
                tracing::info!(
                    migration_number,
                    "migration finalized; resuming commit pipeline"
                );
            }

            output.send_and_record(item, &state_reporter)?;
        }
    }
}
