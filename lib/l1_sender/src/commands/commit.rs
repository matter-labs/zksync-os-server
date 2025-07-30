use crate::commands::L1SenderCommand;
use crate::model::{BatchEnvelope, FriProof};
use alloy::primitives::U256;
use alloy::sol_types::{SolCall, SolValue};
use itertools::Itertools;
use itertools::MinMaxResult::{MinMax, OneElement};
use zksync_os_contract_interface::IExecutor;
#[derive(Debug)]
pub struct CommitCommand {
    input: BatchEnvelope<FriProof>,
}

impl CommitCommand {
    pub fn new(input: BatchEnvelope<FriProof>) -> Self {
        Self { input }
    }
}

impl L1SenderCommand for CommitCommand {
    const NAME: &'static str = "commit";

    fn solidity_call(&self) -> impl SolCall {
        IExecutor::commitBatchesSharedBridgeCall::new((
            U256::from(self.input.batch.commit_batch_info.chain_id),
            U256::from(self.input.batch_number()),
            U256::from(self.input.batch_number()),
            self.to_calldata_suffix().into(),
        ))
    }

    fn into_output_envelope(mut self) -> Vec<BatchEnvelope<FriProof>> {
        self.input.trace = self.input.trace.with_stage("l1 committed");
        vec![self.input]
    }

    fn short_description(&self) -> String {
        format!("commit batch {}", self.input.batch_number())
    }

    fn vec_fmt_debug(input: &[Self]) -> String {
        let minmax = input.iter().map(|cmd| cmd.input.batch_number()).minmax();
        match minmax {
            OneElement(elem) => format!("commit batch {elem}"),
            MinMax(f, t) => format!("commits batches {f}-{t}"),
            _ => "".into(),
        }
    }
}
impl CommitCommand {
    /// `commitBatchesSharedBridge` expects the rest of calldata to be of very specific form. This
    /// function makes sure last committed batch and new batch are encoded correctly.
    fn to_calldata_suffix(&self) -> Vec<u8> {
        /// Current commitment encoding version as per protocol.
        const SUPPORTED_ENCODING_VERSION: u8 = 0;

        let stored_batch_info =
            IExecutor::StoredBatchInfo::from(&self.input.batch.previous_stored_batch_info);
        let commit_batch_info =
            IExecutor::CommitBoojumOSBatchInfo::from(self.input.batch.commit_batch_info.clone());
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
