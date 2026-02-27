use alloy::primitives::B256;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use zksync_os_storage_api::PersistedBatch;

#[cfg_attr(not(feature = "server"), rpc(client, namespace = "unstable"))]
#[cfg_attr(feature = "server", rpc(server, client, namespace = "unstable"))]
pub trait UnstableApi {
    #[method(name = "getBatchByBlockNumber")]
    async fn get_batch_by_block_number(&self, block_number: u64) -> RpcResult<PersistedBatch>;

    #[method(name = "getLocalRoot")]
    async fn get_local_root(&self, batch_number: u64) -> RpcResult<B256>;
}
