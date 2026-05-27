use crate::verifier::metrics::BATCH_VERIFICATION_RESPONDER_METRICS;
use crate::verify_batch_wire::{VerificationRequest, normalized_commit_data};
use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use std::str::FromStr;
use tokio::sync::{broadcast, mpsc};
use zksync_os_batch_types::{BatchSignature, ExtendedCommitBatchInfo};
use zksync_os_contract_interface::l1_discovery::{BatchVerificationSL, L1State};
use zksync_os_network::{
    PeerVerifyBatch, PeerVerifyBatchResult, VerifyBatch, VerifyBatchOutcome, VerifyBatchResult,
};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_tree_block_cache::{CachedBlockNotification, LocalBatchDataCache};

mod metrics;

/// Batch verification responder that consumes requests from the network.
pub struct BatchVerificationResponder {
    chain_id: u64,
    diamond_proxy_sl: Address,
    l1_state: L1State,
    signer: PrivateKeySigner,
    local_batch_data_cache: LocalBatchDataCache,
    verify_request_rx: mpsc::Receiver<PeerVerifyBatch>,
    outgoing_verify_results: broadcast::Sender<PeerVerifyBatchResult>,
}

#[derive(Debug, thiserror::Error)]
enum BatchVerificationError {
    #[error("Missing records for block {0}")]
    MissingBlock(u64),
    #[error("Batch data mismatch")]
    BatchDataMismatch,
    #[error("Local batch data unavailable: {0}")]
    LocalBatchData(anyhow::Error),
}

impl BatchVerificationResponder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain_id: u64,
        diamond_proxy_sl: Address,
        private_key: SecretString,
        l1_state: L1State,
        local_batch_data_cache: LocalBatchDataCache,
        verify_request_rx: mpsc::Receiver<PeerVerifyBatch>,
        outgoing_verify_results: broadcast::Sender<PeerVerifyBatchResult>,
    ) -> Self {
        let signer = PrivateKeySigner::from_str(private_key.expose_secret())
            .expect("Invalid batch verification private key");
        if let BatchVerificationSL::Enabled(l1_config) = l1_state.batch_verification.clone()
            && !l1_config.validators.contains(&signer.address())
        {
            tracing::warn!(
                address = %signer.address(),
                "Your address is not authorized to verify batches on L1",
            );
        }

        Self {
            chain_id,
            diamond_proxy_sl,
            l1_state,
            signer,
            local_batch_data_cache,
            verify_request_rx,
            outgoing_verify_results,
        }
    }

    async fn handle_verification_request(
        &self,
        request: VerificationRequest,
    ) -> Result<BatchSignature, BatchVerificationError> {
        tracing::info!(
            batch_number = request.batch_number,
            request_id = request.request_id,
            "Handling batch verification request (blocks {}-{})",
            request.first_block_number,
            request.last_block_number,
        );

        let blocks = self
            .local_batch_data_cache
            .get_range(request.first_block_number..=request.last_block_number)
            .map_err(BatchVerificationError::LocalBatchData)?
            .ok_or(BatchVerificationError::MissingBlock(
                request.last_block_number,
            ))?;

        let last = blocks.last().unwrap();
        let multichain_root = last.multichain_root;
        let last_replay_record = &last.record;

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
            multichain_root,
            &blocks.first().unwrap().record.protocol_version,
            &last_replay_record.block_context.block_hashes.0,
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
}

#[async_trait]
impl PipelineComponent for BatchVerificationResponder {
    type Input = CachedBlockNotification;
    type Output = ();

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::BatchVerificationResponder;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        _output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        tracing::info!("starting batch verification responder");
        loop {
            state_reporter.enter_state(GenericComponentState::Idle);
            tokio::select! {
                block = input.recv() => {
                    match block {
                        Some(notification) => {
                            state_reporter.enter_state(GenericComponentState::Active);
                            if let Some((start, end)) = self.local_batch_data_cache.range() {
                                BATCH_VERIFICATION_RESPONDER_METRICS.update_cache_range(start, end);
                            }
                            state_reporter.record_processed(notification.block_number, Some(notification.block_timestamp), None);
                        }
                        None => return Ok(()),
                    }
                }
                request = self.verify_request_rx.recv() => {
                    let Some(request) = request else {
                        return Ok(());
                    };
                    state_reporter.enter_state(GenericComponentState::Active);
                    let peer_id = request.peer_id;
                    let request_id = request.message.request_id;
                    let batch_number = request.message.batch_number;
                    let result = self.handle_verification_message(request.message).await?;
                    tracing::info!(%peer_id, request_id, batch_number, "handled batch verification request");
                    let _ = self.outgoing_verify_results.send(PeerVerifyBatchResult {
                        peer_id,
                        message: result,
                    });
                }
            }
        }
    }
}
