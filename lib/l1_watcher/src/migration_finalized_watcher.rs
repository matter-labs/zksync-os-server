use crate::gateway_migration_watcher::GatewayMigrationState;
use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessRawEvents, util};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Log, Topic, ValueOrArray};
use alloy::sol_types::SolEvent;
use tokio::sync::watch;
use zksync_os_contract_interface::{Bridgehub, IChainAssetHandler::MigrationFinalized, ZkChain};

/// Limit the number of SL blocks to scan when performing the initial binary search.
const INITIAL_LOOKBEHIND_BLOCKS: u64 = 100_000;

/// Watches for `MigrationFinalized(uint256 indexed chainId, uint256 migrationNumber, ...)` events
/// emitted by the `IChainAssetHandler` contract on the current settlement layer.
///
/// When detected, transitions the shared [`GatewayMigrationState`] back to
/// [`Stable`][GatewayMigrationState::Stable], allowing the [`MigrationGate`] to resume
/// forwarding L1 commit transactions.
///
/// `MigrationFinalized` has `chainId` as an indexed parameter, so a `topic1` filter is applied
/// to receive only events for this chain.
pub struct MigrationFinalizedWatcher {
    chain_asset_handler: Address,
    /// L2 chain ID used for topic1 filtering.
    l2_chain_id: u64,
    migration_state: watch::Sender<GatewayMigrationState>,
}

impl MigrationFinalizedWatcher {
    /// Creates a watcher that starts scanning from the first SL block where
    /// `IChainAssetHandler::migrationNumber(l2_chain_id) >= current_migration_number`,
    /// determined via binary search.
    ///
    /// `zk_chain` is used for the binary search and its provider is the settlement layer provider.
    /// `bridgehub_sl` is used to retrieve the `IChainAssetHandler` address on the SL.
    pub async fn create_watcher(
        zk_chain: ZkChain<DynProvider>,
        bridgehub_sl: Bridgehub<DynProvider>,
        l2_chain_id: u64,
        l1_chain_id: u64,
        current_migration_number: u64,
        config: L1WatcherConfig,
        migration_state: watch::Sender<GatewayMigrationState>,
    ) -> anyhow::Result<L1Watcher> {
        let chain_asset_handler = bridgehub_sl.chain_asset_handler_address().await?;

        let current_sl_block = zk_chain.provider().get_block_number().await?;
        let starting_block = util::find_block_by_migration_number(
            zk_chain.clone(),
            chain_asset_handler,
            l2_chain_id,
            current_migration_number,
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
            "migration finalized watcher starting"
        );

        L1Watcher::new(
            zk_chain.provider().clone(),
            starting_block,
            config.max_blocks_to_process,
            config.confirmations,
            l1_chain_id,
            config.poll_interval,
            Box::new(Self {
                chain_asset_handler,
                l2_chain_id,
                migration_state,
            }),
        )
        .await
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

    fn contract_addresses(&self) -> ValueOrArray<Address> {
        self.chain_asset_handler.into()
    }

    fn filter_events(&self, logs: Vec<Log>) -> Vec<Log> {
        logs
    }

    /// Filter by `chainId` (topic1) so we only receive events for this chain.
    fn topic1_filter(&self) -> Option<B256> {
        Some(B256::from(U256::from(self.l2_chain_id)))
    }

    async fn process_raw_event(&mut self, log: Log) -> Result<(), L1WatcherError> {
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

        tracing::info!(
            migration_number,
            "MigrationFinalized event received; resuming L1 commit pipeline"
        );

        // Ignore errors: they only occur if every receiver has been dropped.
        let _ = self.migration_state.send(GatewayMigrationState::Stable);
        Ok(())
    }
}
