use alloy::primitives::U64;
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use zksync_os_rpc_api::net::NetApiServer;

pub struct NetNamespace {
    chain_id: u64,
}

impl NetNamespace {
    pub fn new(chain_id: u64) -> Self {
        Self { chain_id }
    }
}

#[async_trait]
impl NetApiServer for NetNamespace {
    async fn version(&self) -> RpcResult<Option<U64>> {
        Ok(Some(U64::from(self.chain_id)))
    }
}
