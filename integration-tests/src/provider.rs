use std::time::Duration;

use crate::network::Zksync;
use alloy::eips::BlockId;
use alloy::primitives::{Address, TxHash};
use alloy::providers::Provider;
use alloy::transports::TransportResult;
use anyhow::Context as _;
use zksync_os_rpc_api::types::L2ToL1LogProof;

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

/// Helper trait to implement additional functionality for tests on top of ZKsync provider.
#[allow(async_fn_in_trait)]
pub trait ZksyncTestingProvider: Provider<Zksync> {
    /// Will wait until the given block is finalized.
    /// This method can hang if the specified block is never produced, so it's recommended
    /// to use `wait_finalized_with_timeout` instead.
    async fn wait_finalized(&self, block_number: u64) -> anyhow::Result<()> {
        tracing::info!("Waiting for block {block_number} to be finalized on L1");
        loop {
            let finalized_block = self.get_block_number_by_id(BlockId::finalized()).await?;
            if finalized_block >= Some(block_number) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Will wait until the given block is finalized, or timeout occurs.
    async fn wait_finalized_with_timeout(
        &self,
        block_number: u64,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        tokio::time::timeout(timeout, self.wait_finalized(block_number))
            .await
            .with_context(|| format!("Block {block_number} was not finalized on L1"))??;
        Ok(())
    }
}

impl<P> ZksyncTestingProvider for P where P: Provider<Zksync> {}
