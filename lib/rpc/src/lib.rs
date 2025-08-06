mod call_fees;

mod config;
pub use config::RpcConfig;

mod eth_call_handler;
mod eth_filter_impl;
mod eth_impl;
mod eth_pubsub_impl;
mod metrics;
mod ots_impl;
mod result;
mod sandbox;
mod tx_handler;
mod types;
mod zks_impl;

use crate::eth_filter_impl::EthFilterNamespace;
use crate::eth_impl::EthNamespace;
use crate::eth_pubsub_impl::EthPubsubNamespace;
use crate::ots_impl::OtsNamespace;
use crate::zks_impl::ZksNamespace;
use alloy::primitives::Address;
use anyhow::Context;
use hyper::Method;
use jsonrpsee::RpcModule;
use jsonrpsee::server::{ServerBuilder, ServerConfigBuilder};
use tower_http::cors::{Any, CorsLayer};
use zksync_os_mempool::L2TransactionPool;
use zksync_os_rpc_api::eth::EthApiServer;
use zksync_os_rpc_api::filter::EthFilterApiServer;
use zksync_os_rpc_api::ots::OtsApiServer;
use zksync_os_rpc_api::pubsub::EthPubSubApiServer;
use zksync_os_rpc_api::zks::ZksApiServer;
use zksync_os_state::StateHandle;
use zksync_os_storage_api::notifications::SubscribeToBlocks;
use zksync_os_storage_api::{ReadReplay, ReadRepository};

pub async fn run_jsonrpsee_server<
    Repository: ReadRepository + SubscribeToBlocks + Clone,
    ReplayStorage: ReadReplay,
    Mempool: L2TransactionPool,
>(
    config: RpcConfig,
    chain_id: u64,
    bridgehub_address: Address,

    repository_manager: Repository,
    replay_storage: ReplayStorage,
    state_handle: StateHandle,
    mempool: Mempool,
) -> anyhow::Result<()> {
    tracing::info!("Starting JSON-RPC server at {}", config.address);

    let mut rpc = RpcModule::new(());
    rpc.merge(
        EthNamespace::new(
            config.clone(),
            repository_manager.clone(),
            replay_storage,
            state_handle.clone(),
            mempool.clone(),
            chain_id,
        )
        .into_rpc(),
    )?;
    rpc.merge(
        EthFilterNamespace::new(config.clone(), repository_manager.clone(), mempool.clone())
            .into_rpc(),
    )?;
    rpc.merge(EthPubsubNamespace::new(repository_manager.clone(), mempool).into_rpc())?;
    rpc.merge(ZksNamespace::new(bridgehub_address).into_rpc())?;
    rpc.merge(OtsNamespace::new(repository_manager, state_handle).into_rpc())?;

    // Add a CORS middleware for handling HTTP requests.
    // This middleware does affect the response, including appropriate
    // headers to satisfy CORS. Because any origins are allowed, the
    // "Access-Control-Allow-Origin: *" header is appended to the response.
    let cors = CorsLayer::new()
        // Allow `POST` when accessing the resource
        .allow_methods([Method::POST])
        // Allow requests from any origin
        .allow_origin(Any)
        .allow_headers([hyper::header::CONTENT_TYPE]);
    let middleware = tower::ServiceBuilder::new().layer(cors);

    let server_config = ServerConfigBuilder::default()
        .max_connections(config.max_connections)
        .build();
    let server_builder = ServerBuilder::default()
        .set_config(server_config)
        .set_http_middleware(middleware);
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
