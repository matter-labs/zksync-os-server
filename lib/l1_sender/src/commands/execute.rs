use crate::commands::SendToL1;
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::sol_types::{SolCall, SolValue};
use std::fmt::Display;
use zksync_os_batch_types::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_batcher_metrics::BatchExecutionStage;
use zksync_os_contract_interface::models::PriorityOpsBatchInfo;
use zksync_os_contract_interface::{IExecutor, InteropRoot};

#[derive(Debug)]
pub struct ExecuteCommand {
    batches: Vec<SignedBatchEnvelope<FriProof>>,
    priority_ops: Vec<PriorityOpsBatchInfo>,
    interop_roots: Vec<Vec<InteropRoot>>,
}

impl ExecuteCommand {
    pub fn new(
        batches: Vec<SignedBatchEnvelope<FriProof>>,
        priority_ops: Vec<PriorityOpsBatchInfo>,
        interop_roots: Vec<Vec<InteropRoot>>,
    ) -> Self {
        assert_eq!(batches.len(), priority_ops.len());
        Self {
            batches,
            priority_ops,
            interop_roots,
        }
    }
}

impl SendToL1 for ExecuteCommand {
    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::L1SenderExecute;
    const SENT_STAGE: BatchExecutionStage = BatchExecutionStage::ExecuteL1TxSent;
    const MINED_STAGE: BatchExecutionStage = BatchExecutionStage::ExecuteL1TxMined;

    const PASSTHROUGH_STAGE: BatchExecutionStage = BatchExecutionStage::ExecuteL1Passthrough;

    fn solidity_call(&self, operator: &Address) -> Bytes {
        IExecutor::executeBatchesSharedBridgeCall::new((
            self.batches.first().unwrap().batch.chain_address,
            U256::from(self.batches.first().unwrap().batch_number()),
            U256::from(self.batches.last().unwrap().batch_number()),
            self.to_calldata_suffix(operator).into(),
        ))
        .abi_encode()
        .into()
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
    fn to_calldata_suffix(&self, operator: &Address) -> Vec<u8> {
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
        let interop_roots = self.interop_roots.clone();

        let protocol_version_minor = self
            .batches
            .first()
            .unwrap()
            .batch
            .batch_info
            .protocol_version
            .minor;
        let encoded_data: Vec<u8> = match protocol_version_minor {
            29 | 30 => (stored_batch_infos, priority_ops, interop_roots).abi_encode_params(),
            31 | 32 => {
                // Batch logs / messages / multichain roots are only relayed when executing on a
                // Gateway; when settling on L1 they are always empty.
                let logs: Vec<Vec<IExecutor::L2Log>> = Vec::new();
                let messages: Vec<Vec<Vec<u8>>> = Vec::new();
                let multichain_roots: Vec<B256> = Vec::new();
                (
                    stored_batch_infos,
                    priority_ops,
                    interop_roots,
                    logs,
                    messages,
                    multichain_roots,
                    operator,
                )
                    .abi_encode_params()
            }
            _ => panic!("Unsupported protocol version: {}", protocol_version_minor),
        };

        /// Current commitment encoding version as per protocol.
        const SUPPORTED_ENCODING_VERSION: u8 = 1;

        // Prefixed by current encoding version as expected by protocol
        [vec![SUPPORTED_ENCODING_VERSION], encoded_data]
            .concat()
            .to_vec()
    }
}
