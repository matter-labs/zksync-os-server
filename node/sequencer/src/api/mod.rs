mod eth_call_handler;
mod eth_impl;
mod metrics;
mod tx_handler;

use crate::api::eth_impl::EthNamespace;
use crate::block_replay_storage::BlockReplayStorage;
use crate::config::RpcConfig;
use crate::finality::FinalityTracker;
use crate::repositories::RepositoryManager;
use alloy::eips::{BlockId, BlockNumberOrTag};
use anyhow::Context;
use jsonrpsee::server::{ServerBuilder, ServerConfigBuilder};
use jsonrpsee::RpcModule;
use zksync_os_mempool::DynPool;
use zksync_os_rpc_api::eth::EthApiServer;
use zksync_os_state::StateHandle;

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

    let server_config = ServerConfigBuilder::default()
        .max_connections(config.max_connections)
        .http_only()
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

// todo: consider best place for this logic - maybe `FinalityInfo` itself?
pub fn resolve_block_id(block: Option<BlockId>, finality_info: &FinalityTracker) -> u64 {
    let block_id: BlockId = block.unwrap_or(BlockId::Number(BlockNumberOrTag::Pending));

    match block_id {
        BlockId::Hash(_) => unimplemented!(),
        // TODO: return last sealed block here when available instead
        BlockId::Number(BlockNumberOrTag::Pending) => finality_info.get_canonized_block(),
        BlockId::Number(BlockNumberOrTag::Latest) => finality_info.get_canonized_block(),
        // TODO: return last committed block here when available instead
        BlockId::Number(BlockNumberOrTag::Safe) => finality_info.get_canonized_block(),
        // TODO: return last executed block here when available instead
        BlockId::Number(BlockNumberOrTag::Finalized) => finality_info.get_canonized_block(),
        BlockId::Number(BlockNumberOrTag::Earliest) => unimplemented!(),
        BlockId::Number(BlockNumberOrTag::Number(number)) => {
            // note: we don't check whether the requested Block Number is less than `BLOCKS_TO_RETAIN` behind -
            // we won't be able to serve `eth_call`s and `storage_at`s for it
            // this will be handled when instantiating `StorageView` for it
            number
        }
    }
}
