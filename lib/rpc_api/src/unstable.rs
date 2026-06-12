use alloy::primitives::B256;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use serde::{Deserialize, Serialize};
use zksync_os_storage_api::{PersistedBatch, ReplayRecord};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SealedReplayRecord {
    pub hash: B256,
    pub record: ReplayRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SealedReplayHead {
    pub block_number: u64,
    pub hash: B256,
}

#[cfg_attr(not(feature = "server"), rpc(client, namespace = "unstable"))]
#[cfg_attr(feature = "server", rpc(server, client, namespace = "unstable"))]
pub trait UnstableApi {
    #[method(name = "getBatchByBlockNumber")]
    fn get_batch_by_block_number(&self, block_number: u64) -> RpcResult<PersistedBatch>;

    #[method(name = "getLocalRoot", blocking)]
    fn get_local_root(&self, batch_number: u64) -> RpcResult<B256>;

    #[method(name = "getReplayRecord")]
    fn get_replay_record(&self, block_number: u64) -> RpcResult<Option<SealedReplayRecord>>;

    #[method(name = "getReplayHead")]
    fn get_replay_head(&self) -> RpcResult<SealedReplayHead>;
}
