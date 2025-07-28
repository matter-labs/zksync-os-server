mod call_fees;
mod eth_call_handler;
mod eth_filter_impl;
mod eth_impl;
mod eth_pubsub_impl;
mod metrics;
mod result;
mod tx_handler;
mod types;

use crate::api::eth_filter_impl::EthFilterNamespace;
use crate::api::eth_impl::EthNamespace;
use crate::api::eth_pubsub_impl::EthPubsubNamespace;
use crate::block_replay_storage::BlockReplayStorage;
use crate::config::{GenesisConfig, RpcConfig};
use crate::repositories::RepositoryManager;
use crate::reth_state::ZkClient;
use alloy::primitives::Address;
use anyhow::Context;
use jsonrpsee::RpcModule;
use jsonrpsee::server::{ServerBuilder, ServerConfigBuilder};
use zksync_os_mempool::RethPool;
use zksync_os_rpc::zks_impl::ZksNamespace;
use zksync_os_rpc_api::eth::EthApiServer;
use zksync_os_rpc_api::filter::EthFilterApiServer;
use zksync_os_rpc_api::pubsub::EthPubSubApiServer;
use zksync_os_rpc_api::zks::ZksApiServer;
use zksync_os_state::StateHandle;

// stripped-down version of `api_server/src/web3/mod.rs`
pub async fn run_jsonrpsee_server(
    config: RpcConfig,
    genesis_config: GenesisConfig,
    bridgehub_address: Address,

    repository_manager: RepositoryManager,
    state_handle: StateHandle,
    mempool: RethPool<ZkClient>,
    block_replay_storage: BlockReplayStorage,
) -> anyhow::Result<()> {
    tracing::info!("Starting JSON-RPC server at {}", config.address);

    let mut rpc = RpcModule::new(());
    rpc.merge(
        EthNamespace::new(
            config.clone(),
            repository_manager.clone(),
            state_handle,
            mempool.clone(),
            block_replay_storage,
            genesis_config.chain_id,
        )
        .into_rpc(),
    )?;
    rpc.merge(
        EthFilterNamespace::new(config.clone(), repository_manager.clone(), mempool.clone())
            .into_rpc(),
    )?;
    rpc.merge(EthPubsubNamespace::new(repository_manager, mempool).into_rpc())?;
    rpc.merge(ZksNamespace::new(bridgehub_address).into_rpc())?;

    let server_config = ServerConfigBuilder::default()
        .max_connections(config.max_connections)
        .build();
    let server_builder = ServerBuilder::default().set_config(server_config);
    // .set_http_middleware(middleware)
    // .max_response_body_size(response_body_size_limit)
    // .set_batch_request_config(batch_request_config)
    // .set_rpc_middleware(rpc_middleware);

    let server = server_builder
        .build(config.address)
        .await
        .context("Failed building HTTP JSON-RPC server")?;

    let server_handle = server.start(rpc);

    server_handle.stopped().await;
    Ok(())
}
