use crate::batcher_metrics::BatchExecutionStage;
use crate::batcher_model::{BatchEnvelope, FriProof};
use crate::commands::L1SenderCommand;
use crate::commitment::StoredBatchInfo;
use alloy::primitives::U256;
use alloy::sol_types::{SolCall, SolValue};
use std::fmt::Display;
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

    fn solidity_call(&self) -> impl SolCall {
        IExecutor::executeBatchesSharedBridgeCall::new((
            U256::from(0),
            U256::from(self.batches.first().unwrap().batch_number()),
            U256::from(self.batches.last().unwrap().batch_number()),
            self.to_calldata_suffix().into(),
        ))
    }

    fn l1_tx_sent_hook(&mut self) {
        self.batches
            .iter_mut()
            .for_each(|batch| batch.set_stage(BatchExecutionStage::ExecuteL1TxSent));
    }

    fn into_output_envelope(self) -> Vec<BatchEnvelope<FriProof>> {
        self.batches
            .into_iter()
            .map(|batch| batch.with_stage(BatchExecutionStage::ExecuteL1TxMined))
            .collect()
    }

    fn display_vec(input: &[Self]) -> String {
        input
            .iter()
            .map(|x| {
                format!(
                    "{}-{}",
                    x.batches.first().unwrap().batch_number(),
                    x.batches.last().unwrap().batch_number()
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
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
