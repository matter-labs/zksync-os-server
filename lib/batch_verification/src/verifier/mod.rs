use crate::verifier::metrics::BATCH_VERIFICATION_RESPONDER_METRICS;
use crate::verify_batch_wire::{VerificationRequest, normalized_commit_data};
use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use secrecy::{ExposeSecret, SecretString};
use std::str::FromStr;
use tokio::sync::{broadcast, mpsc};
use zksync_os_batch_types::{BatchSignature, ExtendedCommitBatchInfo};
use zksync_os_contract_interface::l1_discovery::{BatchVerificationSL, L1State};
use zksync_os_l1_consistency_checker::LocalBatchDataCacheReader;
use zksync_os_network::{
    PeerVerifyBatch, PeerVerifyBatchResult, VerifyBatch, VerifyBatchOutcome, VerifyBatchResult,
};

mod metrics;

/// Batch verification responder that consumes requests from the network.
pub struct BatchVerificationResponder {
    chain_id: u64,
    diamond_proxy_sl: Address,
    l1_state: L1State,
    signer: PrivateKeySigner,
    block_cache: LocalBatchDataCacheReader,
    verify_request_rx: mpsc::Receiver<PeerVerifyBatch>,
    outgoing_verify_results: broadcast::Sender<PeerVerifyBatchResult>,
}

#[derive(Debug, thiserror::Error)]
enum BatchVerificationError {
    #[error("Missing records for block {0}")]
    MissingBlock(u64),
    #[error("Batch data mismatch")]
    BatchDataMismatch,
}

impl BatchVerificationResponder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain_id: u64,
        diamond_proxy_sl: Address,
        private_key: SecretString,
        l1_state: L1State,
        block_cache: LocalBatchDataCacheReader,
        verify_request_rx: mpsc::Receiver<PeerVerifyBatch>,
        outgoing_verify_results: broadcast::Sender<PeerVerifyBatchResult>,
    ) -> Self {
        let signer = PrivateKeySigner::from_str(private_key.expose_secret())
            .expect("Invalid batch verification private key");
        if let BatchVerificationSL::Enabled(l1_config) = l1_state.batch_verification.clone()
            && !l1_config.validators.contains(&signer.address())
        {
            tracing::warn!(
                "Your address {} is not authorized to verify batches on L1",
                signer.address()
            );
        }

        Self {
            chain_id,
            diamond_proxy_sl,
            l1_state,
            signer,
            block_cache,
            verify_request_rx,
            outgoing_verify_results,
        }
    }

    async fn handle_verification_request(
        &self,
        request: VerificationRequest,
    ) -> Result<BatchSignature, BatchVerificationError> {
        tracing::info!(
            "Handling batch verification request {} for batch #{} (blocks {}-{})",
            request.request_id,
            request.batch_number,
            request.first_block_number,
            request.last_block_number,
        );

        let blocks = self
            .block_cache
            .wait_for_range(request.first_block_number..=request.last_block_number)
            .await
            .map_err(|err| {
                tracing::warn!(
                    "failed to load local batch data for verification request {} for batch #{}: {err}",
                    request.request_id,
                    request.batch_number
                );
                BatchVerificationError::MissingBlock(request.first_block_number)
            })?;

        let last_block = blocks.last().unwrap();

        let (batch_info, _) = ExtendedCommitBatchInfo::build(
            blocks
                .iter()
                .map(|block| {
                    (
                        &block.output,
                        block.record.transactions.as_slice(),
                        &block.tree_output,
                    )
                })
                .collect(),
            self.chain_id,
            request.batch_number,
            request.pubdata_mode,
            self.l1_state.sl_chain_id,
            last_block.multichain_root,
            &blocks.first().unwrap().record.protocol_version,
            &last_block.record.block_context.block_hashes.0,
        );

        let expected_commit_data = normalized_commit_data(
            batch_info.commit_info.clone(),
            request.execution_protocol_version,
        );
        if expected_commit_data != request.commit_data {
            return Err(BatchVerificationError::BatchDataMismatch);
        }

        let signature = BatchSignature::sign_batch(
            &request.prev_commit_data,
            &batch_info.commit_info,
            self.diamond_proxy_sl,
            self.l1_state.sl_chain_id,
            self.l1_state.validator_timelock_sl,
            &blocks.first().unwrap().record.protocol_version,
            &self.signer,
        )
        .await;

        Ok(signature)
    }

    async fn handle_verification_message(
        &self,
        request: VerifyBatch,
    ) -> Result<VerifyBatchResult, anyhow::Error> {
        let request_id = request.request_id;
        let batch_number = request.batch_number;
        let request = VerificationRequest::try_from(request)?;
        let result = match self.handle_verification_request(request).await {
            Ok(signature) => {
                BATCH_VERIFICATION_RESPONDER_METRICS
                    .record_request_success(request_id, batch_number);
                VerifyBatchOutcome::Approved(signature.into_raw().to_vec().into())
            }
            Err(reason) => {
                BATCH_VERIFICATION_RESPONDER_METRICS
                    .record_request_failure(request_id, batch_number);
                VerifyBatchOutcome::Refused(reason.to_string())
            }
        };
        Ok(VerifyBatchResult {
            request_id,
            batch_number,
            result,
        })
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        tracing::info!("starting batch verification responder");
        loop {
            let Some(request) = self.verify_request_rx.recv().await else {
                return Ok(());
            };
            let peer_id = request.peer_id;
            let request_id = request.message.request_id;
            let batch_number = request.message.batch_number;
            let result = self.handle_verification_message(request.message).await?;
            tracing::info!(
                "handled batch verification request {} for batch #{} from peer {}",
                request_id,
                batch_number,
                peer_id
            );
            let _ = self.outgoing_verify_results.send(PeerVerifyBatchResult {
                peer_id,
                message: result,
            });
        }
    }
}
