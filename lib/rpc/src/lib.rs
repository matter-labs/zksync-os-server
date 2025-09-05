mod call_fees;

mod config;

use std::error::Error;

pub use config::RpcConfig;

mod eth_call_handler;
mod eth_filter_impl;
mod eth_impl;
mod eth_pubsub_impl;
mod metrics;
mod ots_impl;
mod result;
mod rpc_storage;
pub use rpc_storage::{ReadRpcStorage, RpcStorage};
use tower::util::BoxCloneService;
mod debug_impl;
mod sandbox;
mod tx_handler;
mod types;
mod zks_impl;

use crate::debug_impl::DebugNamespace;
use crate::eth_filter_impl::EthFilterNamespace;
use crate::eth_impl::EthNamespace;
use crate::eth_pubsub_impl::EthPubsubNamespace;
use crate::ots_impl::OtsNamespace;
use crate::zks_impl::ZksNamespace;
use alloy::{primitives::Address, rlp::Bytes};
use hyper::{Method, body::Body};
use jsonrpsee::{
    RpcModule,
    core::BoxError,
    server::{HttpBody, HttpRequest, stop_channel},
};
use tower_http::{
    cors::{Any, CorsLayer},
    limit::{RequestBodyLimitLayer, ResponseBody},
};
use zksync_os_mempool::L2TransactionPool;
use zksync_os_rpc_api::debug::DebugApiServer;
use zksync_os_rpc_api::eth::EthApiServer;
use zksync_os_rpc_api::filter::EthFilterApiServer;
use zksync_os_rpc_api::ots::OtsApiServer;
use zksync_os_rpc_api::pubsub::EthPubSubApiServer;
use zksync_os_rpc_api::zks::ZksApiServer;

pub fn build_jsonrpsee_service<
    RpcStorage: ReadRpcStorage,
    Mempool: L2TransactionPool,
    RequestBody,
>(
    config: RpcConfig,
    chain_id: u64,
    bridgehub_address: Address,
    storage: RpcStorage,
    mempool: Mempool,
) -> anyhow::Result<
    BoxCloneService<
        HttpRequest<RequestBody>,
        hyper::Response<ResponseBody<HttpBody>>,
        Box<(dyn Error + Send + Sync)>,
    >,
>
where
    RequestBody: Body<Data = Bytes> + Send + 'static,
    RequestBody::Error: Into<BoxError>,
{
    let mut rpc = RpcModule::new(());
    rpc.merge(
        EthNamespace::new(config.clone(), storage.clone(), mempool.clone(), chain_id).into_rpc(),
    )?;
    rpc.merge(
        EthFilterNamespace::new(config.clone(), storage.clone(), mempool.clone()).into_rpc(),
    )?;
    rpc.merge(EthPubsubNamespace::new(storage.clone(), mempool).into_rpc())?;
    rpc.merge(ZksNamespace::new(bridgehub_address, storage.clone()).into_rpc())?;
    rpc.merge(OtsNamespace::new(storage.clone()).into_rpc())?;
    rpc.merge(DebugNamespace::new(storage).into_rpc())?;

    let (stop_handle, _server_handle) = stop_channel();
    let service = jsonrpsee::server::Server::builder()
        .to_service_builder()
        .build(rpc, stop_handle);

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

    // TODO: limit response size
    let rpc_service = tower::ServiceBuilder::new()
        .concurrency_limit(config.max_connections)
        .layer(RequestBodyLimitLayer::new(config.max_request_size_bytes()))
        .layer(cors)
        .service(service);

    Ok(BoxCloneService::new(rpc_service))
}
