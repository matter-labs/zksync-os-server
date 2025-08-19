use crate::batcher_metrics::BatchExecutionStage;
use crate::batcher_model::{BatchEnvelope, FriProof, SnarkProof};
use crate::commands::L1SenderCommand;
use crate::commitment::StoredBatchInfo;
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use alloy::primitives::{B256, U256, keccak256};
use alloy::sol_types::SolCall;
use std::collections::HashMap;
use std::fmt::Display;
use vise::{Counter, LabeledFamily};
use zksync_os_contract_interface::IExecutor;
use zksync_os_contract_interface::IExecutor::{proofPayloadCall, proveBatchesSharedBridgeCall};

const OHBENDER_PROOF_TYPE: i32 = 2;
const FAKE_PROOF_TYPE: i32 = 3;
const FAKE_PROOF_MAGIC_VALUE: i32 = 13;

#[derive(Debug)]
pub struct ProofCommand {
    batches: Vec<BatchEnvelope<FriProof>>,
    // only fake proof is supported for now
    proof: SnarkProof,
}

impl ProofCommand {
    pub fn new(batches: Vec<BatchEnvelope<FriProof>>, proof: SnarkProof) -> Self {
        Self { batches, proof }
    }
}

impl L1SenderCommand for ProofCommand {
    const NAME: &'static str = "prove";
    const SENT_STAGE: BatchExecutionStage = BatchExecutionStage::ProveL1TxSent;
    const MINED_STAGE: BatchExecutionStage = BatchExecutionStage::ProveL1TxMined;

    fn state_metric() -> &'static LabeledFamily<L1SenderState, Counter<f64>> {
        &L1_SENDER_METRICS.prove_state
    }

    fn solidity_call(&self) -> impl SolCall {
        proveBatchesSharedBridgeCall::new((
            U256::from(0),
            U256::from(self.batches.first().unwrap().batch_number()),
            U256::from(self.batches.last().unwrap().batch_number()),
            self.to_calldata_suffix().into(),
        ))
    }
}

impl AsRef<[BatchEnvelope<FriProof>]> for ProofCommand {
    fn as_ref(&self) -> &[BatchEnvelope<FriProof>] {
        self.batches.as_slice()
    }
}

impl AsMut<[BatchEnvelope<FriProof>]> for ProofCommand {
    fn as_mut(&mut self) -> &mut [BatchEnvelope<FriProof>] {
        self.batches.as_mut_slice()
    }
}

impl From<ProofCommand> for Vec<BatchEnvelope<FriProof>> {
    fn from(value: ProofCommand) -> Self {
        value.batches
    }
}

impl Display for ProofCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "prove batches {}-{}",
            self.batches.first().unwrap().batch_number(),
            self.batches.last().unwrap().batch_number()
        )?;
        Ok(())
    }
}

impl ProofCommand {
    fn shift_b256_right(input: &B256) -> B256 {
        let mut bytes = [0_u8; 32];
        bytes[4..32].copy_from_slice(&input.as_slice()[0..28]);
        B256::from_slice(&bytes)
    }

    fn get_batch_public_input(prev_batch: &StoredBatchInfo, batch: &StoredBatchInfo) -> B256 {
        let mut bytes = Vec::with_capacity(32 * 3);
        bytes.extend_from_slice(prev_batch.state_commitment.as_slice());
        bytes.extend_from_slice(batch.state_commitment.as_slice());
        bytes.extend_from_slice(batch.commitment.as_slice());
        keccak256(&bytes)
    }
    fn snark_public_input(previous_batch: &StoredBatchInfo, batches: &[StoredBatchInfo]) -> B256 {
        let mut hash_map: HashMap<usize, &StoredBatchInfo> = HashMap::new();
        hash_map.insert(previous_batch.batch_number as usize, previous_batch);
        for batch in batches {
            hash_map.insert(batch.batch_number as usize, batch);
        }
        let start = batches.first().unwrap().batch_number as usize;
        let end = batches.last().unwrap().batch_number as usize;

        // taken from https://github.com/mm-zk/zksync_tools/blob/cf2c47d61fa8399a030d0b31d4396832f802489b/prove_execute/src/main.rs
        let mut result: Option<B256> = None;
        for i in start..=end {
            let batch = hash_map.get(&i).expect("Batch not found");
            let prev_batch = hash_map.get(&(i - 1)).expect("Previous batch not found");
            let public_input = Self::get_batch_public_input(prev_batch, batch);
            // Snark public input is public_input >> 32.
            let snark_input = Self::shift_b256_right(&public_input);

            match result {
                Some(ref mut res) => {
                    // Combine with previous result.
                    let mut combined = [0_u8; 64];
                    combined[..32].copy_from_slice(&res.0);
                    combined[32..].copy_from_slice(&snark_input.0);
                    *res = Self::shift_b256_right(&keccak256(combined));
                }
                None => {
                    result = Some(snark_input);
                }
            }
        }
        result.unwrap()
    }
    fn to_calldata_suffix(&self) -> Vec<u8> {
        let previous_batch_info = &self
            .batches
            .first()
            .unwrap()
            .batch
            .previous_stored_batch_info;
        let stored_batch_infos: Vec<StoredBatchInfo> = self
            .batches
            .iter()
            .map(|batch| StoredBatchInfo::from(batch.batch.commit_batch_info.clone()))
            .collect();

        // todo: remove tostring
        let public_input = Self::snark_public_input(previous_batch_info, &stored_batch_infos);

        tracing::info!(">> public input: {}", public_input);

        let proof: Vec<U256> = match &self.proof {
            SnarkProof::Fake => {
                vec![
                    // Fake proof type
                    U256::from(FAKE_PROOF_TYPE),
                    // OhBender 'previous hash' - for fake proof, we can always assume that it matches the range perfectly.
                    U256::from(0),
                    // Fake proof magic value (just for sanity)
                    U256::from(FAKE_PROOF_MAGIC_VALUE),
                    // Public input (fake proof **will** verify this against batch data stored in the contract)
                    U256::from_be_bytes(public_input.0),
                ]
            }
            SnarkProof::Real(bytes) => {
                let proof: Vec<U256> = bytes
                    .chunks(32)
                    .map(|chunk| {
                        let arr: [u8; 32] = chunk
                            .try_into()
                            .expect("proof bytes must be a multiple of 32");
                        U256::from_be_bytes(arr)
                    })
                    .collect();
                vec![
                    // Fake proof type
                    U256::from(OHBENDER_PROOF_TYPE),
                    // we generate SNARK proofs to always match the range perfectly.
                    U256::from(0),
                ]
                .into_iter()
                .chain(proof)
                .collect()
            }
        };

        let proof_payload = proofPayloadCall {
            old: IExecutor::StoredBatchInfo::from(previous_batch_info),
            newInfo: stored_batch_infos
                .iter()
                .map(Into::into) // into `IExecutor::StoredBatchInfo`
                .collect(),
            proof,
        };

        const SUPPORTED_ENCODING_VERSION: u8 = 0;

        let mut proof_data = vec![SUPPORTED_ENCODING_VERSION];
        proof_payload.abi_encode_raw(&mut proof_data);
        proof_data
    }
}
