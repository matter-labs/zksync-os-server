use crate::batcher_metrics::BatchExecutionStage;
use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::SendToL1;
use alloy::primitives::U256;
use alloy::sol_types::{SolCall, SolValue};
use std::fmt::Display;
use zksync_os_contract_interface::IExecutor;
use zksync_os_contract_interface::models::BatchDaInputMode;

#[derive(Debug)]
pub struct CommitCommand {
    input: SignedBatchEnvelope<FriProof>,
    da_input_mode: BatchDaInputMode,
}

impl CommitCommand {
    pub fn new(input: SignedBatchEnvelope<FriProof>, da_input_mode: BatchDaInputMode) -> Self {
        Self {
            input,
            da_input_mode,
        }
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
        /// Current commitment encoding version for ZKsync OS.
        const SUPPORTED_ENCODING_VERSION: u8 = 2;

        let stored_batch_info =
            IExecutor::StoredBatchInfo::from(&self.input.batch.previous_stored_batch_info);
        let mut batch_info = self.input.batch.batch_info.clone();
        // `BatchInfo` has full da input - even for validium chains we only drop `operator_da_input`
        // field when we are actually committing the batch this way, we don't need to consider the DA
        // mode in advance - it's only known to the l1-sender.
        batch_info.commit_info.operator_da_input = match self.da_input_mode {
            BatchDaInputMode::Rollup => batch_info.commit_info.operator_da_input,
            BatchDaInputMode::Validium => U256::ZERO.to_be_bytes_vec(),
        };
        let commit_batch_info = IExecutor::CommitBatchInfoZKsyncOS::from(batch_info.commit_info);
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
