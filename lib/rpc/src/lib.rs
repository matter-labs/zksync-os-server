mod call_fees;

mod config;

pub use config::RpcConfig;
use std::sync::Arc;

mod eth_call_handler;
mod eth_filter_impl;
mod eth_impl;
mod eth_pubsub_impl;
mod metrics;
mod ots_impl;
mod result;
mod rpc_storage;
pub use rpc_storage::{ReadRpcStorage, RpcStorage};
mod debug_impl;
mod monitoring_middleware;
mod net_impl;
mod sandbox;
mod tx_handler;
mod types;
mod web3_impl;
mod zks_impl;

use crate::debug_impl::DebugNamespace;
use crate::eth_call_handler::EthCallHandler;
use crate::eth_filter_impl::EthFilterNamespace;
use crate::eth_impl::EthNamespace;
use crate::eth_pubsub_impl::EthPubsubNamespace;
use crate::monitoring_middleware::Monitoring;
use crate::net_impl::NetNamespace;
use crate::ots_impl::OtsNamespace;
use crate::web3_impl::Web3Namespace;
use crate::zks_impl::ZksNamespace;
use alloy::primitives::Address;
use anyhow::Context;
use hyper::Method;
use jsonrpsee::RpcModule;
use jsonrpsee::server::{ServerBuilder, ServerConfigBuilder};
use jsonrpsee::ws_client::RpcServiceBuilder;
use tower_http::cors::{Any, CorsLayer};
use zksync_os_genesis::GenesisInputSource;
use zksync_os_mempool::L2TransactionPool;
use zksync_os_rpc_api::debug::DebugApiServer;
use zksync_os_rpc_api::eth::EthApiServer;
use zksync_os_rpc_api::filter::EthFilterApiServer;
use zksync_os_rpc_api::net::NetApiServer;
use zksync_os_rpc_api::ots::OtsApiServer;
use zksync_os_rpc_api::pubsub::EthPubSubApiServer;
use zksync_os_rpc_api::web3::Web3ApiServer;
use zksync_os_rpc_api::zks::ZksApiServer;

pub async fn run_jsonrpsee_server<RpcStorage: ReadRpcStorage, Mempool: L2TransactionPool>(
    config: RpcConfig,
    chain_id: u64,
    bridgehub_address: Address,
    storage: RpcStorage,
    mempool: Mempool,
    genesis_input_source: Arc<dyn GenesisInputSource>,
) -> anyhow::Result<()> {
    tracing::info!("Starting JSON-RPC server at {}", config.address);

    let mut rpc = RpcModule::new(());
    let eth_call_handler = EthCallHandler::new(config.clone(), storage.clone(), chain_id);
    rpc.merge(
        EthNamespace::new(
            storage.clone(),
            mempool.clone(),
            eth_call_handler.clone(),
            chain_id,
        )
        .into_rpc(),
    )?;
    rpc.merge(
        EthFilterNamespace::new(config.clone(), storage.clone(), mempool.clone()).into_rpc(),
    )?;
    rpc.merge(EthPubsubNamespace::new(storage.clone(), mempool).into_rpc())?;
    rpc.merge(
        ZksNamespace::new(bridgehub_address, storage.clone(), genesis_input_source).into_rpc(),
    )?;
    rpc.merge(OtsNamespace::new(storage.clone()).into_rpc())?;
    rpc.merge(DebugNamespace::new(storage.clone(), eth_call_handler).into_rpc())?;
    rpc.merge(NetNamespace::new(chain_id).into_rpc())?;
    rpc.merge(Web3Namespace.into_rpc())?;

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

    let max_response_size_bytes = config.max_response_size_bytes();
    let rpc_middleware = RpcServiceBuilder::new()
        .layer_fn(move |service| Monitoring::new(service, max_response_size_bytes));

    let server_config = ServerConfigBuilder::default()
        .max_connections(config.max_connections)
        .max_request_body_size(config.max_request_size_bytes())
        .max_response_body_size(config.max_response_size_bytes())
        .build();
    let server_builder = ServerBuilder::default()
        .set_config(server_config)
        .set_http_middleware(middleware)
        .set_rpc_middleware(rpc_middleware);

    let server = server_builder
        .build(config.address)
        .await
        .context("Failed building HTTP JSON-RPC server")?;

    let server_handle = server.start(rpc);

    server_handle.stopped().await;
    Ok(())
}
