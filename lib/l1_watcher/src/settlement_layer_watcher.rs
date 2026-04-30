use alloy::eips::BlockId;
use alloy::primitives::Address;
use alloy::providers::DynProvider;
use std::time::Duration;
use tokio::sync::watch;
use zksync_os_contract_interface::ZkChain;

/// Polls `getSettlementLayer()` on the L1 diamond proxy and terminates the process once all three
/// conditions are simultaneously satisfied:
///
/// 1. **Settlement layer changed**: `getSettlementLayer()` ≠ initial value.
/// 2. **Gate triggered**: [`MigrationGate`][crate::MigrationGate] has forwarded the
///    `SetSLChainId` batch (trigger batch number N is known via `migration_triggered`).
/// 3. **Predecessors executed**: every batch before N is fully executed on L1
///    (`get_total_batches_executed() ≥ N − 1`).
///
/// Waiting for all three conditions prevents the node from restarting prematurely — before the
/// old settlement layer has finalised all the batches that were in-flight at migration time.
pub struct SettlementLayerWatcher {
    diamond_proxy_l1: ZkChain<DynProvider>,
    /// Value of `getSettlementLayer()` at the time the node started.
    initial_settlement_layer: Address,
    poll_interval: Duration,
    /// Receives the trigger batch number from `MigrationGate` once it has detected and forwarded
    /// the `SetSLChainId` batch. `None` means no migration has been triggered yet.
    migration_triggered: watch::Receiver<Option<u64>>,
}

impl SettlementLayerWatcher {
    pub fn new(
        diamond_proxy_l1: ZkChain<DynProvider>,
        initial_settlement_layer: Address,
        poll_interval: Duration,
        migration_triggered: watch::Receiver<Option<u64>>,
    ) -> Self {
        Self {
            diamond_proxy_l1,
            initial_settlement_layer,
            poll_interval,
            migration_triggered,
        }
    }

    pub async fn run(self) {
        loop {
            tokio::time::sleep(self.poll_interval).await;

            // Condition 1: settlement layer must have changed.
            let current = match self
                .diamond_proxy_l1
                .get_settlement_layer(BlockId::latest())
                .await
            {
                Ok(addr) => addr,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to poll getSettlementLayer(); will retry");
                    continue;
                }
            };
            if current == self.initial_settlement_layer {
                continue;
            }

            // Condition 2: MigrationGate must have forwarded the SetSLChainId batch.
            let Some(trigger_batch_number) = *self.migration_triggered.borrow() else {
                tracing::info!(
                    initial = %self.initial_settlement_layer,
                    current  = %current,
                    "settlement layer changed; waiting for MigrationGate to discover SetSLChainId batch before restarting"
                );
                continue;
            };

            // Condition 3: all batches before the trigger must be executed on L1.
            let executed = match self
                .diamond_proxy_l1
                .get_total_batches_executed(BlockId::latest())
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to poll getTotalBatchesExecuted(); will retry");
                    continue;
                }
            };
            let required = trigger_batch_number.saturating_sub(1);
            if executed < required {
                tracing::info!(
                    executed,
                    required,
                    "settlement layer changed; waiting for preceding batches to be executed on L1 before restarting"
                );
                continue;
            }

            // All conditions met — crash so the process manager restarts us against the new SL.
            tracing::error!(
                initial = %self.initial_settlement_layer,
                current  = %current,
                trigger_batch_number,
                executed,
                "all migration preconditions met; restarting node to reinitialise against new settlement layer"
            );
            std::process::exit(1);
        }
    }
}
