use alloy::primitives::Address;
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use zksync_os_rpc_api::zks::ZksApiServer;

pub struct ZksNamespace {
    bridgehub_address: Address,
}

impl ZksNamespace {
    pub fn new(bridgehub_address: Address) -> Self {
        Self { bridgehub_address }
    }
}

#[async_trait]
impl ZksApiServer for ZksNamespace {
    async fn get_bridgehub_contract(&self) -> RpcResult<Address> {
        Ok(self.bridgehub_address)
    }
}
