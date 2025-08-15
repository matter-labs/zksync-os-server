use crate::network::Zksync;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::transports::TransportResult;

/// RPC interface that gives access to methods specific to ZKsync OS.
#[allow(async_fn_in_trait)]
pub trait ZksyncApi: Provider<Zksync> {
    async fn get_bridgehub_contract(&self) -> TransportResult<Address> {
        self.client().request("zks_getBridgehubContract", ()).await
    }
}

impl<P> ZksyncApi for P where P: Provider<Zksync> {}
