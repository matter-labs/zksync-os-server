use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessRawEvents, util};
use alloy::primitives::{Address, B256, BlockNumber, ChainId, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Log, Topic, ValueOrArray};
use alloy::sol_types::SolEvent;
use std::sync::Arc;
use zksync_os_contract_interface::ServerNotifier::MigrateFromGateway;
use zksync_os_contract_interface::{
    Bridgehub, IChainAssetHandler, ServerNotifier::MigrateToGateway, ZkChain,
};
use zksync_os_mempool::subpools::sl_chain_id::SlChainIdSubpool;
use zksync_os_types::SystemTxEnvelope;

/// Limit the number of L1 blocks to scan when looking for the migration number block.
const INITIAL_LOOKBEHIND_BLOCKS: u64 = 100_000;

/// Watches for both `MigrateToGateway` and `MigrateFromGateway` events on L1 in a single
/// polling loop, and submits a `SetSLChainId` system transaction for each.
///
/// - `MigrateToGateway` (L1 → GW): new SL = `gw_chain_id`.
/// - `MigrateFromGateway` (GW → L1): new SL = `l1_chain_id`.
pub struct GatewayMigrationWatcher {
    server_notifier_contract: Address,
    /// The L2 chain ID this node belongs to. Passed as topic1 in `eth_getLogs` so only
    /// events for this chain are returned by the RPC node.
    l2_chain_id: ChainId,
    /// New settlement layer chain ID when a `MigrateToGateway` event fires.
    gw_chain_id: ChainId,
    /// New settlement layer chain ID when a `MigrateFromGateway` event fires.
    l1_chain_id: ChainId,
    sl_chain_id_subpool: SlChainIdSubpool,
}

impl GatewayMigrationWatcher {
    #[allow(clippy::too_many_arguments)]
    pub async fn create_watcher(
        zk_chain: ZkChain<DynProvider>,
        bridgehub: Bridgehub<DynProvider>,
        l2_chain_id: ChainId,
        l1_chain_id: ChainId,
        gw_chain_id: ChainId,
        current_migration_number: u64,
        config: L1WatcherConfig,
        sl_chain_id_subpool: SlChainIdSubpool,
    ) -> anyhow::Result<L1Watcher> {
        let server_notifier_contract = zk_chain.get_server_notifier_address().await?;
        let chain_asset_handler_address = bridgehub.chain_asset_handler_address().await?;

        let current_l1_block = zk_chain.provider().get_block_number().await?;
        let next_l1_block = find_l1_block_by_migration_number(
            zk_chain.clone(),
            chain_asset_handler_address,
            l2_chain_id,
            current_migration_number,
        )
        .await
        .or_else(|err| {
            if current_l1_block > INITIAL_LOOKBEHIND_BLOCKS {
                anyhow::bail!(
                    "Binary search failed with {err}. Cannot default starting block to zero \
                     for a long chain. Current L1 block number: {current_l1_block}. \
                     Limit: {INITIAL_LOOKBEHIND_BLOCKS}."
                );
            } else {
                Ok(0)
            }
        })?;

        tracing::info!(
            contract = %server_notifier_contract,
            starting_l1_block = next_l1_block,
            l1_chain_id,
            gw_chain_id,
            "gateway migration watcher starting"
        );

        let this = Self {
            server_notifier_contract,
            l2_chain_id,
            l1_chain_id,
            gw_chain_id,
            sl_chain_id_subpool,
        };

        L1Watcher::new(
            zk_chain.provider().clone(),
            next_l1_block,
            config.max_blocks_to_process,
            config.confirmations,
            l1_chain_id,
            config.poll_interval,
            Box::new(this),
        )
        .await
    }
}

#[async_trait::async_trait]
impl ProcessRawEvents for GatewayMigrationWatcher {
    fn name(&self) -> &'static str {
        "gateway_migration"
    }

    fn event_signatures(&self) -> Topic {
        Topic::default()
            .extend(MigrateToGateway::SIGNATURE_HASH)
            .extend(MigrateFromGateway::SIGNATURE_HASH)
    }

    fn contract_addresses(&self) -> ValueOrArray<Address> {
        self.server_notifier_contract.into()
    }

    fn filter_events(&self, logs: Vec<Log>) -> Vec<Log> {
        logs
    }

    fn topic1_filter(&self) -> Option<B256> {
        // Filter by the indexed chainId topic so the RPC node returns only events for our chain.
        Some(B256::from(U256::from(self.l2_chain_id)))
    }

    async fn process_raw_event(&mut self, log: Log) -> Result<(), L1WatcherError> {
        let Some(&topic0) = log.topic0() else {
            return Ok(());
        };

        let (new_sl_chain_id, migration_number) = match topic0 {
            MigrateToGateway::SIGNATURE_HASH => {
                let event = MigrateToGateway::decode_log(&log.inner)?.data;
                let migration_number: u64 = event.migrationNumber.try_into().unwrap();
                (self.gw_chain_id, migration_number)
            }
            MigrateFromGateway::SIGNATURE_HASH => {
                let event = MigrateFromGateway::decode_log(&log.inner)?.data;
                let migration_number: u64 = event.migrationNumber.try_into().unwrap();
                (self.l1_chain_id, migration_number)
            }
            _ => {
                return Err(L1WatcherError::Other(anyhow::anyhow!(
                    "Unexpected event with topic0 {topic0:#x} in gateway migration watcher"
                )));
            }
        };

        tracing::info!(
            new_sl_chain_id,
            migration_number,
            "gateway migration event caught"
        );

        let envelope = SystemTxEnvelope::set_sl_chain_id(new_sl_chain_id, migration_number);
        self.sl_chain_id_subpool.insert(envelope).await;
        Ok(())
    }
}

/// Finds the first L1 block where `migrationNumber(_chainId) >= migration_number` on the
/// `IChainAssetHandler` contract, using binary search. This is used to determine the starting
/// L1 block for the gateway migration watcher.
async fn find_l1_block_by_migration_number(
    zk_chain: ZkChain<DynProvider>,
    chain_asset_handler: Address,
    chain_id: u64,
    migration_number: u64,
) -> anyhow::Result<BlockNumber> {
    let instance = Arc::new(IChainAssetHandler::new(
        chain_asset_handler,
        zk_chain.provider().clone(),
    ));
    let target = U256::from(migration_number);

    util::find_l1_block_by_predicate(Arc::new(zk_chain), 0, move |zk, block| {
        let instance = instance.clone();
        async move {
            let code = zk
                .provider()
                .get_code_at(*instance.address())
                .block_id(block.into())
                .await?;
            if code.0.is_empty() {
                return Ok(false);
            }
            let res = instance
                .migrationNumber(U256::from(chain_id))
                .block(block.into())
                .call()
                .await?;
            Ok(res >= target)
        }
    })
    .await
}
