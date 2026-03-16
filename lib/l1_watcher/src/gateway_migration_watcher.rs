use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event, util};
use alloy::primitives::{Address, B256, BlockNumber, ChainId, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;
use std::marker::PhantomData;
use std::sync::Arc;
use zksync_os_contract_interface::ServerNotifier::MigrateFromGateway;
use zksync_os_contract_interface::{
    Bridgehub, IChainAssetHandler, ServerNotifier::MigrateToGateway, ZkChain,
};
use zksync_os_mempool::subpools::sl_chain_id::SlChainIdSubpool;
use zksync_os_types::SystemTxEnvelope;

/// Limit the number of L1 blocks to scan when looking for the migration number block.
const INITIAL_LOOKBEHIND_BLOCKS: u64 = 100_000;

pub trait MigrationProcessor: Send + Sync + 'static {
    type Event: SolEvent + Send + Sync + 'static;

    fn migration_number(event: &Self::Event) -> u64;
}

pub struct Gateway;

pub struct L1;

impl MigrationProcessor for Gateway {
    type Event = MigrateFromGateway;

    fn migration_number(event: &Self::Event) -> u64 {
        event.migrationNumber.try_into().unwrap()
    }
}

impl MigrationProcessor for L1 {
    type Event = MigrateToGateway;

    fn migration_number(event: &Self::Event) -> u64 {
        event.migrationNumber.try_into().unwrap()
    }
}

pub struct GatewayMigrationWatcher<T> {
    server_notifier_contract: Address,
    /// The L2 chain ID this node belongs to. Passed as topic1 in `eth_getLogs` so only
    /// events for this chain are returned by the RPC node.
    l2_chain_id: ChainId,
    /// The chain ID of the new settlement layer this watcher is watching migrations toward.
    /// Used as the argument to `setSettlementLayerChainId` when an event fires.
    new_sl_chain_id: ChainId,
    sl_chain_id_subpool: SlChainIdSubpool,

    _marker: PhantomData<T>,
}

impl<T: MigrationProcessor> GatewayMigrationWatcher<T> {
    pub async fn create_watcher(
        zk_chain: ZkChain<DynProvider>,
        bridgehub: Bridgehub<DynProvider>,
        chain_id: u64,
        new_sl_chain_id: ChainId,
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
            chain_id,
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
            "gateway migration watcher starting"
        );

        let this = Self {
            server_notifier_contract,
            l2_chain_id: chain_id,
            new_sl_chain_id,
            sl_chain_id_subpool,
            _marker: PhantomData,
        };

        let l1_watcher = L1Watcher::new(
            zk_chain.provider().clone(),
            next_l1_block,
            config.max_blocks_to_process,
            config.poll_interval,
            this.into(),
        );
        Ok(l1_watcher)
    }
}

#[async_trait::async_trait]
impl<T: MigrationProcessor> ProcessL1Event for GatewayMigrationWatcher<T> {
    const NAME: &'static str = "gateway_migration";

    type SolEvent = T::Event;
    type WatchedEvent = T::Event;

    fn contract_address(&self) -> Address {
        self.server_notifier_contract
    }

    fn topic1_filter(&self) -> Option<B256> {
        // Filter by the indexed chainId topic so the RPC node returns only events for our chain.
        Some(B256::from(U256::from(self.l2_chain_id)))
    }

    async fn process_event(&mut self, tx: T::Event, _log: Log) -> Result<(), L1WatcherError> {
        let migration_number = T::migration_number(&tx);

        tracing::info!(
            new_sl_chain_id = self.new_sl_chain_id,
            migration_number,
            "gateway migration event caught"
        );

        let envelope = SystemTxEnvelope::set_sl_chain_id(self.new_sl_chain_id, migration_number);

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
