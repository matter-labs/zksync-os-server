use crate::batcher_metrics::BatchExecutionStage;
use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::SendToL1;
use alloy::consensus::BlobTransactionSidecar;
use alloy::primitives::{Bytes, U256};
use alloy::sol_types::{SolCall, SolValue};
use std::fmt::Display;
use zksync_os_contract_interface::calldata::encode_commit_batch_data;
use zksync_os_contract_interface::models::PriorityOpsBatchInfo;
use zksync_os_contract_interface::{IExecutor, IPermissionlessValidator, InteropRoot};

use super::prove::encode_prove_calldata_suffix;

#[derive(Debug)]
pub struct ExecuteCommand {
    batches: Vec<SignedBatchEnvelope<FriProof>>,
    priority_ops: Vec<PriorityOpsBatchInfo>,
    pub settle_mode: bool,
}

impl ExecuteCommand {
    pub fn new(
        batches: Vec<SignedBatchEnvelope<FriProof>>,
        priority_ops: Vec<PriorityOpsBatchInfo>,
        settle_mode: bool,
    ) -> Self {
        assert_eq!(batches.len(), priority_ops.len());
        Self {
            batches,
            priority_ops,
            settle_mode,
        }
    }
}

impl SendToL1 for ExecuteCommand {
    const NAME: &'static str = "execute";
    const SENT_STAGE: BatchExecutionStage = BatchExecutionStage::ExecuteL1TxSent;
    const MINED_STAGE: BatchExecutionStage = BatchExecutionStage::ExecuteL1TxMined;

    const PASSTHROUGH_STAGE: BatchExecutionStage = BatchExecutionStage::ExecuteL1Passthrough;
    const SETTLE_PASSTHROUGH: bool = false;

    fn solidity_call(&self, gateway: bool) -> Bytes {
        if self.settle_mode {
            return self.settle_solidity_call(gateway);
        }
        IExecutor::executeBatchesSharedBridgeCall::new((
            self.batches.first().unwrap().batch.batch_info.chain_address,
            U256::from(self.batches.first().unwrap().batch_number()),
            U256::from(self.batches.last().unwrap().batch_number()),
            self.to_calldata_suffix(gateway).into(),
        ))
        .abi_encode()
        .into()
    }

    fn blob_sidecar(&self) -> Option<BlobTransactionSidecar> {
        if self.settle_mode {
            // In settle mode, return the blob sidecar from the commit stage
            self.batches
                .first()
                .and_then(|b| b.batch.commit_blob_sidecar.clone())
        } else {
            None
        }
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
    fn settle_solidity_call(&self, gateway: bool) -> Bytes {
        let first_batch = self.batches.first().unwrap();
        let chain_address = first_batch.batch.batch_info.chain_address;
        let batch_from = U256::from(first_batch.batch_number());
        let batch_to = U256::from(self.batches.last().unwrap().batch_number());

        // Build commit data
        let commit_data = encode_commit_batch_data(
            &first_batch.batch.previous_stored_batch_info,
            first_batch.batch.batch_info.commit_info.clone(),
            first_batch.batch.protocol_version.minor,
        );

        // Build prove data using the SNARK proof stored in the batch during passthrough
        let snark_proof = first_batch.batch.snark_proof.as_ref().expect(
            "settle mode requires snark_proof in batch metadata. \
             This can happen if already-proved batches are passed through without \
             prepare_settle_passthrough() being called (e.g., when transitioning from \
             normal mode to settle mode with last_proved_batch > last_executed_batch). \
             Ensure the startup validation in run_main_node_pipeline catches this state.",
        );
        let prove_data = encode_prove_calldata_suffix(&self.batches, snark_proof);

        // Build execute data
        let execute_data = self.to_calldata_suffix(gateway);

        IPermissionlessValidator::settleBatchesSharedBridgeCall::new((
            chain_address,
            batch_from,
            batch_to,
            commit_data.into(),
            prove_data.into(),
            execute_data.into(),
        ))
        .abi_encode()
        .into()
    }

    fn to_calldata_suffix(&self, gateway: bool) -> Vec<u8> {
        let stored_batch_infos = self
            .batches
            .iter()
            .map(|batch| {
                batch
                    .batch
                    .batch_info
                    .clone()
                    .into_stored(&batch.batch.protocol_version)
            })
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

        let encoded_data: Vec<u8> = match self.batches.first().unwrap().batch.protocol_version.minor
        {
            29 | 30 => (stored_batch_infos, priority_ops, interop_roots).abi_encode_params(),
            31 | 32 => {
                let mut logs = Vec::new();
                let mut messages = Vec::new();
                let mut multichain_roots = Vec::new();
                if gateway {
                    logs = self
                        .batches
                        .iter()
                        .map(|batch| {
                            batch
                                .batch
                                .logs
                                .iter()
                                .cloned()
                                .map(IExecutor::L2Log::from)
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>();
                    messages = self
                        .batches
                        .iter()
                        .map(|batch| batch.batch.messages.clone())
                        .collect::<Vec<_>>();
                    multichain_roots = self
                        .batches
                        .iter()
                        .map(|batch| batch.batch.multichain_root)
                        .collect::<Vec<_>>();
                }
                (
                    stored_batch_infos,
                    priority_ops,
                    interop_roots,
                    logs,
                    messages,
                    multichain_roots,
                )
                    .abi_encode_params()
            }
            _ => panic!(
                "Unsupported protocol version: {}",
                self.batches.first().unwrap().batch.protocol_version
            ),
        };

        /// Current commitment encoding version as per protocol.
        const SUPPORTED_ENCODING_VERSION: u8 = 1;

        // Prefixed by current encoding version as expected by protocol
        [vec![SUPPORTED_ENCODING_VERSION], encoded_data]
            .concat()
            .to_vec()
    }
}
