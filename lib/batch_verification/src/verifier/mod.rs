use crate::verifier::metrics::BATCH_VERIFICATION_RESPONDER_METRICS;
use crate::verify_batch_wire::{VerificationRequest, normalized_commit_data};
use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use secrecy::{ExposeSecret, SecretString};
use std::str::FromStr;
use tokio::sync::{broadcast, mpsc, watch};
use zksync_os_batch_types::BatchSignature;
use zksync_os_contract_interface::l1_discovery::{BatchVerificationSL, L1State};
use zksync_os_l1_consistency_checker::BatchReplayer;
use zksync_os_network::{
    PeerVerifyBatch, PeerVerifyBatchResult, VerifyBatch, VerifyBatchOutcome, VerifyBatchResult,
};
use zksync_os_storage_api::{ReadReplay, ReadStateHistory};

mod metrics;

/// Batch verification responder that consumes requests from the network.
pub struct BatchVerificationResponder<State, Replays> {
    diamond_proxy_sl: Address,
    l1_state: L1State,
    signer: PrivateKeySigner,
    replayer: BatchReplayer<State, Replays>,
    /// Highest block processed by the local pipeline (published by the L1 consistency checker);
    /// requests are served once it passes the requested range.
    last_processed_block: watch::Receiver<u64>,
    verify_request_rx: mpsc::Receiver<PeerVerifyBatch>,
    outgoing_verify_results: broadcast::Sender<PeerVerifyBatchResult>,
}

#[derive(Debug, thiserror::Error)]
enum BatchVerificationError {
    #[error("Missing records for block {0}")]
    MissingBlock(u64),
    #[error("Failed to rebuild batch locally: {0:#}")]
    Rebuild(anyhow::Error),
    #[error("Batch data mismatch")]
    BatchDataMismatch,
}

impl<State: ReadStateHistory + Clone, Replays: ReadReplay + Clone>
    BatchVerificationResponder<State, Replays>
{
    pub fn new(
        diamond_proxy_sl: Address,
        private_key: SecretString,
        l1_state: L1State,
        replayer: BatchReplayer<State, Replays>,
        last_processed_block: watch::Receiver<u64>,
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
            diamond_proxy_sl,
            l1_state,
            signer,
            replayer,
            last_processed_block,
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

        // Wait until the local pipeline has processed the requested range; everything needed to
        // rebuild the batch is then available in storage.
        self.last_processed_block
            .clone()
            .wait_for(|&block| block >= request.last_block_number)
            .await
            .map_err(|_| BatchVerificationError::MissingBlock(request.last_block_number))?;

        let replayer = self.replayer.clone();
        let range = request.first_block_number..=request.last_block_number;
        let (batch_number, pubdata_mode) = (request.batch_number, request.pubdata_mode);
        // Rebuilding re-executes every block of the batch in the VM; keep that off the async
        // runtime.
        let batch_info = tokio::task::spawn_blocking(move || {
            replayer.build_batch_info(range, batch_number, pubdata_mode)
        })
        .await
        .map_err(|err| BatchVerificationError::Rebuild(err.into()))?
        .map_err(|err| {
            tracing::warn!(
                "failed to rebuild local batch data for verification request {} for batch #{}: {err:#}",
                request.request_id,
                request.batch_number
            );
            BatchVerificationError::Rebuild(err)
        })?;

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
            &batch_info.protocol_version,
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
