use crate::batcher_metrics::BatchExecutionStage;
use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::SendToL1;
use alloy::primitives::U256;
use alloy::sol_types::{SolCall, SolValue};
use std::fmt::Display;
use zksync_os_contract_interface::models::PriorityOpsBatchInfo;
use zksync_os_contract_interface::{IExecutor, InteropRoot};

#[derive(Debug)]
pub struct ExecuteCommand {
    batches: Vec<SignedBatchEnvelope<FriProof>>,
    priority_ops: Vec<PriorityOpsBatchInfo>,
}

impl ExecuteCommand {
    pub fn new(
        batches: Vec<SignedBatchEnvelope<FriProof>>,
        priority_ops: Vec<PriorityOpsBatchInfo>,
    ) -> Self {
        assert_eq!(batches.len(), priority_ops.len());
        Self {
            batches,
            priority_ops,
        }
    }
}

impl SendToL1 for ExecuteCommand {
    const NAME: &'static str = "execute";
    const SENT_STAGE: BatchExecutionStage = BatchExecutionStage::ExecuteL1TxSent;
    const MINED_STAGE: BatchExecutionStage = BatchExecutionStage::ExecuteL1TxMined;

    const PASSTHROUGH_STAGE: BatchExecutionStage = BatchExecutionStage::ExecuteL1Passthrough;

    fn solidity_call(&self) -> impl SolCall {
        IExecutor::executeBatchesSharedBridgeCall::new((
            self.batches.first().unwrap().batch.batch_info.chain_address,
            U256::from(self.batches.first().unwrap().batch_number()),
            U256::from(self.batches.last().unwrap().batch_number()),
            self.to_calldata_suffix().into(),
        ))
    }
}

impl AsRef<[SignedBatchEnvelope<FriProof>]> for ExecuteCommand {
    fn as_ref(&self) -> &[SignedBatchEnvelope<FriProof>] {
        self.batches.as_slice()
    }
}

impl AsMut<[SignedBatchEnvelope<FriProof>]> for ExecuteCommand {
    fn as_mut(&mut self) -> &mut [SignedBatchEnvelope<FriProof>] {
        self.batches.as_mut_slice()
    }
}

impl From<ExecuteCommand> for Vec<SignedBatchEnvelope<FriProof>> {
    fn from(value: ExecuteCommand) -> Self {
        value.batches
    }
}

impl Display for ExecuteCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "execute batches {}-{}",
            self.batches.first().unwrap().batch_number(),
            self.batches.last().unwrap().batch_number()
        )?;
        Ok(())
    }
}

impl ExecuteCommand {
    fn to_calldata_suffix(&self) -> Vec<u8> {
        let stored_batch_infos = self
            .batches
            .iter()
            .map(|batch| batch.batch.batch_info.clone().into_stored())
            .map(|batch| IExecutor::StoredBatchInfo::from(&batch))
            .collect::<Vec<_>>();
        let priority_ops = self
            .priority_ops
            .iter()
            .cloned()
            .map(IExecutor::PriorityOpsBatchInfo::from)
            .collect::<Vec<_>>();
        // For now interop roots are empty.
        let interop_roots: Vec<Vec<InteropRoot>> = vec![vec![]; self.batches.len()];
        let encoded_data = (stored_batch_infos, priority_ops, interop_roots).abi_encode_params();

        /// Current commitment encoding version as per protocol.
        const SUPPORTED_ENCODING_VERSION: u8 = 1;

        // Prefixed by current encoding version as expected by protocol
        [vec![SUPPORTED_ENCODING_VERSION], encoded_data]
            .concat()
            .to_vec()
    }
}
