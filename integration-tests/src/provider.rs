use crate::network::Zksync;
use alloy::primitives::{Address, TxHash};
use alloy::providers::Provider;
use alloy::transports::TransportResult;
use zksync_os_types::rpc::L2ToL1LogProof;

/// RPC interface that gives access to methods specific to ZKsync OS.
#[allow(async_fn_in_trait)]
pub trait ZksyncApi: Provider<Zksync> {
    async fn get_bridgehub_contract(&self) -> TransportResult<Address> {
        self.client().request("zks_getBridgehubContract", ()).await
    }

    async fn get_l2_to_l1_log_proof(
        &self,
        tx_hash: TxHash,
        index: u64,
    ) -> TransportResult<Option<L2ToL1LogProof>> {
        self.client()
            .request("zks_getL2ToL1LogProof", (tx_hash, index))
            .await
    }
}

impl<P> ZksyncApi for P where P: Provider<Zksync> {}
