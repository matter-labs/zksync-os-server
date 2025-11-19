use crate::batcher_metrics::BatchExecutionStage;
use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::SendToL1;
use alloy::consensus::BlobTransactionSidecar;
use alloy::primitives::U256;
use alloy::sol_types::{SolCall, SolValue};
use std::fmt::Display;
use zksync_os_contract_interface::{IExecutor, IExecutorV29};

#[derive(Debug)]
pub struct CommitCommand {
    input: SignedBatchEnvelope<FriProof>,
}

impl CommitCommand {
    pub fn new(input: SignedBatchEnvelope<FriProof>) -> Self {
        Self { input }
    }

    pub(crate) fn input(&self) -> &SignedBatchEnvelope<FriProof> {
        &self.input
    }
}

impl SendToL1 for CommitCommand {
    const NAME: &'static str = "commit";
    const SENT_STAGE: BatchExecutionStage = BatchExecutionStage::CommitL1TxSent;
    const MINED_STAGE: BatchExecutionStage = BatchExecutionStage::CommitL1TxMined;
    const PASSTHROUGH_STAGE: BatchExecutionStage = BatchExecutionStage::CommitL1Passthrough;

    fn solidity_call(&self) -> impl SolCall {
        IExecutor::commitBatchesSharedBridgeCall::new((
            self.input.batch.batch_info.chain_address,
            U256::from(self.input.batch_number()),
            U256::from(self.input.batch_number()),
            self.to_calldata_suffix().into(),
        ))
    }

    fn blob_sidecar(&self) -> Option<BlobTransactionSidecar> {
        self.input.batch.batch_info.blob_sidecar.clone()
    }
}

impl AsRef<[SignedBatchEnvelope<FriProof>]> for CommitCommand {
    fn as_ref(&self) -> &[SignedBatchEnvelope<FriProof>] {
        std::slice::from_ref(&self.input)
    }
}

impl AsMut<[SignedBatchEnvelope<FriProof>]> for CommitCommand {
    fn as_mut(&mut self) -> &mut [SignedBatchEnvelope<FriProof>] {
        std::slice::from_mut(&mut self.input)
    }
}

impl From<CommitCommand> for Vec<SignedBatchEnvelope<FriProof>> {
    fn from(value: CommitCommand) -> Self {
        vec![value.input]
    }
}

impl Display for CommitCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "commit batch {}", self.input.batch_number())?;
        Ok(())
    }
}

impl CommitCommand {
    /// `commitBatchesSharedBridge` expects the rest of calldata to be of very specific form. This
    /// function makes sure last committed batch and new batch are encoded correctly.
    fn to_calldata_suffix(&self) -> Vec<u8> {
        let stored_batch_info =
            IExecutor::StoredBatchInfo::from(&self.input.batch.previous_stored_batch_info);

        match self.input.batch.protocol_version.minor {
            29 => {
                const V29_ENCODING_VERSION: u8 = 2;

                let commit_batch_info = IExecutorV29::CommitBatchInfoZKsyncOS::from(
                    self.input.batch.batch_info.commit_info.clone(),
                );
                tracing::debug!(
                    last_batch_hash = ?self.input.batch.previous_stored_batch_info.hash(),
                    last_batch_number = ?self.input.batch.previous_stored_batch_info.batch_number,
                    new_batch_number = ?commit_batch_info.batchNumber,
                    "preparing commit calldata"
                );
                let encoded_data = (stored_batch_info, vec![commit_batch_info]).abi_encode_params();

                // Prefixed by current encoding version as expected by protocol
                [[V29_ENCODING_VERSION].to_vec(), encoded_data].concat()
            }
            // 31 needed for upgrade integration test
            30 | 31 => {
                const V30_ENCODING_VERSION: u8 = 3;

                let commit_batch_info = IExecutor::CommitBatchInfoZKsyncOS::from(
                    self.input.batch.batch_info.commit_info.clone(),
                );
                tracing::debug!(
                    last_batch_hash = ?self.input.batch.previous_stored_batch_info.hash(),
                    last_batch_number = ?self.input.batch.previous_stored_batch_info.batch_number,
                    new_batch_number = ?commit_batch_info.batchNumber,
                    "preparing commit calldata"
                );
                let encoded_data = (stored_batch_info, vec![commit_batch_info]).abi_encode_params();

                // Prefixed by current encoding version as expected by protocol
                [[V30_ENCODING_VERSION].to_vec(), encoded_data].concat()
            }
            _ => panic!(
                "Unsupported protocol version: {}",
                self.input.batch.protocol_version
            ),
        }
    }
}
