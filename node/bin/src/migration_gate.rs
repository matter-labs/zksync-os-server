use tokio::sync::mpsc;
use tokio::sync::watch;
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_l1_watcher::GatewayMigrationState;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

/// A pipeline component that acts as a gate in front of the L1 commit sender.
///
/// Under normal operation it is transparent — items flow straight through.
///
/// The gate only activates when it observes a commit batch that contains a `SetSLChainId`
/// system transaction whose migration number matches the current
/// [`InProgress`][GatewayMigrationState::InProgress] state set by
/// [`GatewayMigrationWatcher`][zksync_os_l1_watcher::GatewayMigrationWatcher].
/// Once that triggering batch has been detected, the gate:
/// 1. Signals `migration_triggered` with the batch number so that
///    [`SettlementLayerWatcher`][zksync_os_l1_watcher::SettlementLayerWatcher] can check
///    whether all preceding batches have been executed before crashing the node.
/// 2. Pauses all subsequent batches until
///    [`MigrationFinalizedWatcher`][zksync_os_l1_watcher::MigrationFinalizedWatcher]
///    transitions the shared state back to [`Stable`][GatewayMigrationState::Stable].
pub struct MigrationGate {
    pub migration_state: watch::Receiver<GatewayMigrationState>,
    /// Notifies `SettlementLayerWatcher` of the batch number that contains `SetSLChainId`.
    /// Sent as soon as the triggering batch is detected, before entering the wait.
    pub migration_triggered: watch::Sender<Option<u64>>,
}

#[async_trait::async_trait]
impl PipelineComponent for MigrationGate {
    type Input = L1SenderCommand<CommitCommand>;
    type Output = L1SenderCommand<CommitCommand>;

    const NAME: &'static str = "migration_gate";
    // 1-sized buffer so back-pressure propagates immediately upstream when the gate is closed.
    const OUTPUT_BUFFER_SIZE: usize = 1;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        loop {
            let Some(item) = input.recv().await else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };

            // Check whether this batch contains the `SetSLChainId` transaction for the
            // current migration. Only `SendToL1` batches can trigger the gate; already-committed
            // `Passthrough` batches are forwarded unconditionally.
            let triggering_migration_number = if let L1SenderCommand::SendToL1(command) = &item {
                let migration_state = self.migration_state.borrow().clone();
                if let GatewayMigrationState::InProgress { migration_number } = migration_state {
                    // CommitCommand always contains exactly one envelope; use AsRef to access it.
                    command
                        .as_ref()
                        .first()
                        .and_then(|e| e.batch.set_sl_chain_id_migration_number)
                        .filter(|&n| n == migration_number)
                } else {
                    None
                }
            } else {
                None
            };

            // If this was the triggering batch, signal SettlementLayerWatcher and then pause.
            if let Some(migration_number) = triggering_migration_number {
                let trigger_batch_number = item.first_batch_number();
                tracing::info!(
                    migration_number,
                    trigger_batch_number,
                    "SetSLChainId batch detected; signalling settlement layer watcher and pausing commit pipeline"
                );
                // Signal before waiting so SettlementLayerWatcher can immediately start checking
                // the executed-batch precondition.
                let _ = self.migration_triggered.send(Some(trigger_batch_number));

                self.migration_state
                    .wait_for(|s| *s == GatewayMigrationState::Stable)
                    .await?;
                tracing::info!(
                    migration_number,
                    "migration finalized; resuming commit pipeline"
                );
            }

            if output.send(item).await.is_err() {
                tracing::info!("outbound channel closed");
                return Ok(());
            }
        }
    }
}
