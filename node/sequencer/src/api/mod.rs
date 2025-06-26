mod eth_call_handler;
mod eth_impl;
mod metrics;
mod tx_handler;
use crate::api::eth_impl::EthNamespace;
use crate::block_replay_storage::BlockReplayStorage;
use crate::config::RpcConfig;
use crate::finality::FinalityTracker;
use crate::repositories::RepositoryManager;
use anyhow::Context;
use zksync_os_mempool::DynPool;
use zksync_os_state::StateHandle;
use zksync_types::api::{BlockId, BlockIdVariant, BlockNumber};
use zksync_web3_decl::jsonrpsee::server::ServerBuilder;
use zksync_web3_decl::jsonrpsee::RpcModule;
use zksync_web3_decl::namespaces::EthNamespaceServer;

// stripped-down version of `api_server/src/web3/mod.rs`
pub async fn run_jsonrpsee_server(
    config: RpcConfig,

    repository_manager: RepositoryManager,
    finality_tracker: FinalityTracker,
    state_handle: StateHandle,
    mempool: DynPool,
    block_replay_storage: BlockReplayStorage,
) -> anyhow::Result<()> {
    tracing::info!("Starting JSON-RPC server at {}", config.address);

    let mut rpc = RpcModule::new(());
    rpc.merge(
        EthNamespace::new(
            config.clone(),
            repository_manager,
            finality_tracker,
            state_handle,
            mempool,
            block_replay_storage,
        )
        .into_rpc(),
    )?;

    let server_builder = ServerBuilder::default().max_connections(config.max_connections);
    // .set_http_middleware(middleware)
    // .max_response_body_size(response_body_size_limit)
    // .set_batch_request_config(batch_request_config)
    // .set_rpc_middleware(rpc_middleware);

    let server = server_builder
        .http_only()
        .build(config.address)
        .await
        .context("Failed building HTTP JSON-RPC server")?;

    let server_handle = server.start(rpc);

    server_handle.stopped().await;
    Ok(())
}

// todo: consider best place for this logic - maybe `FinalityInfo` itself?
pub fn resolve_block_id(block: Option<BlockIdVariant>, finality_info: &FinalityTracker) -> u64 {
    let block_id: BlockId = block
        .map(|b| b.into())
        .unwrap_or_else(|| BlockId::Number(BlockNumber::Pending));

    match block_id {
        BlockId::Hash(_) => unimplemented!(),
        // todo (Daniyar): research exact expectations on each BlockNumber
        BlockId::Number(BlockNumber::Pending)
        | BlockId::Number(BlockNumber::Committed)
        | BlockId::Number(BlockNumber::Finalized)
        | BlockId::Number(BlockNumber::Latest)
        | BlockId::Number(BlockNumber::L1Committed) => finality_info.get_canonized_block(),
        BlockId::Number(BlockNumber::Earliest) => unimplemented!(),
        BlockId::Number(BlockNumber::Number(number)) => {
            // note: we don't check whether the requested Block Number is less than `BLOCKS_TO_RETAIN` behind -
            // we won't be able to serve `eth_call`s and `storage_at`s for it
            // this will be handled when instantiating `StorageView` for it
            number.as_u64()
        }
    }
}
