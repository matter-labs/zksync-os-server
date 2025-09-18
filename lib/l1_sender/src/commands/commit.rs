use crate::batcher_metrics::BatchExecutionStage;
use crate::batcher_model::{BatchEnvelope, FriProof};
use crate::commands::L1SenderCommand;
use alloy::primitives::U256;
use alloy::sol_types::{SolCall, SolValue};
use std::fmt::Display;
use zksync_os_contract_interface::IExecutor;
use zksync_os_contract_interface::models::BatchDaInputMode;

#[derive(Debug)]
pub struct CommitCommand {
    input: BatchEnvelope<FriProof>,
    da_input_mode: BatchDaInputMode,
}

impl CommitCommand {
    pub fn new(input: BatchEnvelope<FriProof>, da_input_mode: BatchDaInputMode) -> Self {
        Self {
            input,
            da_input_mode,
        }
    }
}

impl L1SenderCommand for CommitCommand {
    const NAME: &'static str = "commit";
    const SENT_STAGE: BatchExecutionStage = BatchExecutionStage::CommitL1TxSent;
    const MINED_STAGE: BatchExecutionStage = BatchExecutionStage::CommitL1TxMined;
    fn solidity_call(&self) -> impl SolCall {
        IExecutor::commitBatchesSharedBridgeCall::new((
            self.input.batch.commit_batch_info.chain_address,
            U256::from(self.input.batch_number()),
            U256::from(self.input.batch_number()),
            self.to_calldata_suffix().into(),
        ))
    }
}

impl AsRef<[BatchEnvelope<FriProof>]> for CommitCommand {
    fn as_ref(&self) -> &[BatchEnvelope<FriProof>] {
        std::slice::from_ref(&self.input)
    }
}

impl AsMut<[BatchEnvelope<FriProof>]> for CommitCommand {
    fn as_mut(&mut self) -> &mut [BatchEnvelope<FriProof>] {
        std::slice::from_mut(&mut self.input)
    }
}

impl From<CommitCommand> for Vec<BatchEnvelope<FriProof>> {
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
        /// Current commitment encoding version for ZKsync OS.
        const SUPPORTED_ENCODING_VERSION: u8 = 2;

        let stored_batch_info =
            IExecutor::StoredBatchInfo::from(&self.input.batch.previous_stored_batch_info);
        let commit_batch_info = self
            .input
            .batch
            .commit_batch_info
            .clone()
            .into_l1_commit_data(self.da_input_mode);
        tracing::debug!(
            last_batch_hash = ?self.input.batch.previous_stored_batch_info.hash(),
            last_batch_number = ?self.input.batch.previous_stored_batch_info.batch_number,
            new_batch_number = ?commit_batch_info.batchNumber,
            "preparing commit calldata"
        );
        let encoded_data = (stored_batch_info, vec![commit_batch_info]).abi_encode_params();

        // Prefixed by current encoding version as expected by protocol
        [[SUPPORTED_ENCODING_VERSION].to_vec(), encoded_data]
            .concat()
            .to_vec()
    }
}
