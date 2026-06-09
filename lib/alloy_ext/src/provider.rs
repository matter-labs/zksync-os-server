use crate::network::Zksync;
use alloy::primitives::{Address, BlockNumber, StorageKey, TxHash};
use alloy::providers::Provider;
use alloy::transports::TransportResult;
use serde::Deserialize;
use std::time::Duration;
use zksync_os_contract_interface::models::StoredBatchInfo;
use zksync_os_rpc_api::types::{BatchStorageProof, L2ToL1LogProof, LogProofTarget};

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

    async fn get_l2_to_l1_log_proof_with_target(
        &self,
        tx_hash: TxHash,
        index: u64,
        target: LogProofTarget,
    ) -> TransportResult<Option<L2ToL1LogProof>> {
        self.client()
            .request("zks_getL2ToL1LogProof", (tx_hash, index, target))
            .await
    }

    async fn get_storage_proof(
        &self,
        address: Address,
        keys: Vec<StorageKey>,
        batch_number: u64,
    ) -> TransportResult<Option<BatchStorageProof>> {
        self.client()
            .request("zks_getProof", (address, keys, batch_number))
            .await
    }

    async fn get_batch_number_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> TransportResult<u64> {
        #[derive(Debug, Deserialize)]
        struct CommittedBatchView {
            batch_info: StoredBatchInfo,
        }

        let CommittedBatchView { batch_info } = self
            .client()
            .request("unstable_getBatchByBlockNumber", (block_number,))
            .await?;
        tracing::debug!(block_number, ?batch_info, "got batch info for block");
        Ok(batch_info.batch_number)
    }

    async fn wait_batch_number_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> TransportResult<u64> {
        loop {
            match self.get_batch_number_by_block_number(block_number).await {
                Ok(number) => return Ok(number),
                Err(err)
                    if err.as_error_resp().is_some_and(|err| {
                        err.code == -32603 && err.message.contains("has not been finalized")
                    }) =>
                {
                    tracing::info!(block_number, %err, "batch corresponding to block isn't finalized; waiting");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

impl<P> ZksyncApi for P where P: Provider<Zksync> {}
