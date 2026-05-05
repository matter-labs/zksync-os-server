use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessRawEvents, util};
use alloy::primitives::{B256, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Log, Topic};
use alloy::sol_types::SolEvent;
use tokio::sync::watch;
use zksync_os_contract_interface::settlement_layer_intervals::SettlementLayerIntervals;
use zksync_os_contract_interface::{Bridgehub, IChainAssetHandler::MigrationFinalized, ZkChain};

/// Limit the number of SL blocks to scan when performing the initial binary search.
const INITIAL_LOOKBEHIND_BLOCKS: u64 = 100_000;

/// Watches for `MigrationFinalized(uint256 indexed chainId, uint256 migrationNumber, ...)` events
/// emitted by the `IChainAssetHandler` contract on the current settlement layer.
///
/// The watcher is the only writer of `last_finalized_migration`, a monotonic counter consumed by
/// [`MigrationGate`][crate::MigrationGate] to decide whether to forward an L1 commit. Each event
/// raises the counter to `max(current, event.migrationNumber)`.
///
/// `MigrationFinalized` has `chainId` as an indexed parameter, so a `topic1` filter is applied
/// to receive only events for this chain.
pub struct MigrationFinalizedWatcher {
    /// L2 chain ID used for topic1 filtering.
    l2_chain_id: u64,
    last_finalized_migration: watch::Sender<u64>,
}

impl MigrationFinalizedWatcher {
    /// Initializes `last_finalized_migration` from the current SL's
    /// `IChainAssetHandler.migrationNumber(l2_chain_id)` (which by construction tracks the
    /// destination's view of finalized migrations) and decides whether to spawn the watcher.
    pub async fn create_watcher(
        zk_chain: ZkChain<DynProvider>,
        bridgehub_sl: Bridgehub<DynProvider>,
        intervals: &SettlementLayerIntervals,
        l2_chain_id: u64,
        l1_chain_id: u64,
        config: L1WatcherConfig,
        last_finalized_migration: watch::Sender<u64>,
    ) -> anyhow::Result<Option<L1Watcher>> {
        let active_migration_number = (intervals.intervals().len() - 1) as u64;
        let sl_migration_number: u64 = bridgehub_sl
            .migration_number(l2_chain_id)
            .await?
            .try_into()
            .map_err(|e| anyhow::anyhow!("SL migrationNumber overflow: {e}"))?;
        // Seed the counter from the SL so the gate's pause condition starts off correct even
        // before the watcher (if any) processes its first event.
        let _ = last_finalized_migration.send(sl_migration_number);

        if active_migration_number == 0 {
            tracing::info!(
                "no gateway migrations recorded on L1; skipping migration finalized watcher"
            );
            return Ok(None);
        }
        if sl_migration_number >= active_migration_number {
            tracing::info!(
                migration_number = sl_migration_number,
                "current SL interval migration (#{sl_migration_number}) already finalized; skipping migration finalized watcher"
            );
            return Ok(None);
        }

        let chain_asset_handler = bridgehub_sl.chain_asset_handler_address().await?;
        let current_sl_block = zk_chain.provider().get_block_number().await?;
        // todo: not necessary to run binary search here, just use latest
        let starting_block = util::find_block_by_migration_number(
            zk_chain.clone(),
            chain_asset_handler,
            l2_chain_id,
            active_migration_number,
        )
        .await
        .or_else(|err| {
            if current_sl_block > INITIAL_LOOKBEHIND_BLOCKS {
                anyhow::bail!(
                    "Binary search failed with {err}. Cannot default starting block to zero \
                     for a long chain. Current SL block number: {current_sl_block}. \
                     Limit: {INITIAL_LOOKBEHIND_BLOCKS}."
                );
            } else {
                Ok(0)
            }
        })?;

        tracing::info!(
            contract = %chain_asset_handler,
            l2_chain_id,
            starting_block,
            active_migration_number,
            "migration finalized watcher starting"
        );

        let watcher = L1Watcher::new(
            config,
            zk_chain.provider().clone(),
            chain_asset_handler.into(),
            starting_block,
            None,
            l1_chain_id,
            Box::new(Self {
                l2_chain_id,
                last_finalized_migration,
            }),
        )
        .await?;
        Ok(Some(watcher))
    }
}

#[async_trait::async_trait]
impl ProcessRawEvents for MigrationFinalizedWatcher {
    fn name(&self) -> &'static str {
        "migration_finalized"
    }

    fn event_signatures(&self) -> Topic {
        Topic::default().extend(MigrationFinalized::SIGNATURE_HASH)
    }

    fn filter_events(&self, logs: Vec<Log>) -> Vec<Log> {
        logs
    }

    /// Filter by `chainId` (topic1) so we only receive events for this chain.
    fn topic1_filter(&self) -> Option<B256> {
        Some(B256::from(U256::from(self.l2_chain_id)))
    }

    async fn process_raw_event(
        &mut self,
        _provider: &DynProvider,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        let Some(&topic0) = log.topic0() else {
            return Ok(());
        };
        if topic0 != MigrationFinalized::SIGNATURE_HASH {
            return Err(L1WatcherError::Other(anyhow::anyhow!(
                "Unexpected event with topic0 {topic0:#x} in migration finalized watcher"
            )));
        }

        let event = MigrationFinalized::decode_log(&log.inner)
            .map_err(|e| L1WatcherError::Other(e.into()))?
            .data;
        let migration_number: u64 = event
            .migrationNumber
            .try_into()
            .map_err(|e| L1WatcherError::Other(anyhow::anyhow!("migrationNumber overflow: {e}")))?;

        // Monotonic raise: out-of-order or stale events can never lower the counter.
        self.last_finalized_migration.send_if_modified(|current| {
            if migration_number > *current {
                *current = migration_number;
                tracing::info!(
                    migration_number,
                    "MigrationFinalized event observed; advancing last_finalized_migration"
                );
                true
            } else {
                false
            }
        });
        Ok(())
    }
}
