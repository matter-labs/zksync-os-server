use crate::batcher_metrics::BatchExecutionStage;
use crate::batcher_model::{BatchEnvelope, FriProof};
use crate::commands::L1SenderCommand;
use crate::commitment::StoredBatchInfo;
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use alloy::primitives::U256;
use alloy::sol_types::{SolCall, SolValue};
use std::fmt::Display;
use vise::{Counter, LabeledFamily};
use zksync_os_contract_interface::IExecutor;
use zksync_os_contract_interface::models::PriorityOpsBatchInfo;

#[derive(Debug)]
pub struct ExecuteCommand {
    batches: Vec<BatchEnvelope<FriProof>>,
    priority_ops: Vec<PriorityOpsBatchInfo>,
}

impl ExecuteCommand {
    pub fn new(
        batches: Vec<BatchEnvelope<FriProof>>,
        priority_ops: Vec<PriorityOpsBatchInfo>,
    ) -> Self {
        assert_eq!(batches.len(), priority_ops.len());
        Self {
            batches,
            priority_ops,
        }
    }
}

impl L1SenderCommand for ExecuteCommand {
    const NAME: &'static str = "execute";
    const SENT_STAGE: BatchExecutionStage = BatchExecutionStage::ExecuteL1TxSent;
    const MINED_STAGE: BatchExecutionStage = BatchExecutionStage::ExecuteL1TxMined;

    fn state_metric() -> &'static LabeledFamily<L1SenderState, Counter<f64>> {
        &L1_SENDER_METRICS.execute_state
    }

    fn solidity_call(&self) -> impl SolCall {
        IExecutor::executeBatchesSharedBridgeCall::new((
            U256::from(0),
            U256::from(self.batches.first().unwrap().batch_number()),
            U256::from(self.batches.last().unwrap().batch_number()),
            self.to_calldata_suffix().into(),
        ))
    }
}

impl AsRef<[BatchEnvelope<FriProof>]> for ExecuteCommand {
    fn as_ref(&self) -> &[BatchEnvelope<FriProof>] {
        self.batches.as_slice()
    }
}

impl AsMut<[BatchEnvelope<FriProof>]> for ExecuteCommand {
    fn as_mut(&mut self) -> &mut [BatchEnvelope<FriProof>] {
        self.batches.as_mut_slice()
    }
}

impl From<ExecuteCommand> for Vec<BatchEnvelope<FriProof>> {
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
            .map(|batch| StoredBatchInfo::from(batch.batch.commit_batch_info.clone()))
            .map(|batch| IExecutor::StoredBatchInfo::from(&batch))
            .collect::<Vec<_>>();
        let priority_ops = self
            .priority_ops
            .iter()
            .cloned()
            .map(IExecutor::PriorityOpsBatchInfo::from)
            .collect::<Vec<_>>();
        let encoded_data = (stored_batch_infos, priority_ops).abi_encode_params();

        const SUPPORTED_ENCODING_VERSION: u8 = 0;

        // Prefixed by current encoding version as expected by protocol
        [vec![SUPPORTED_ENCODING_VERSION], encoded_data]
            .concat()
            .to_vec()
    }
}
