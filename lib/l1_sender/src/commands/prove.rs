use crate::commands::SendToL1;
use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::sol_types::SolCall;
use std::collections::HashMap;
use std::fmt::Display;
use zksync_os_batch_types::batcher_model::{FriProof, SignedBatchEnvelope, SnarkProof};
use zksync_os_batcher_metrics::BatchExecutionStage;
use zksync_os_contract_interface::IExecutor;
use zksync_os_contract_interface::IExecutor::{proofPayloadCall, proveBatchesSharedBridgeCall};
use zksync_os_contract_interface::models::StoredBatchInfo;

const OHBENDER_PROOF_TYPE: u32 = 2;
const FAKE_PROOF_TYPE: u32 = 3;
const FAKE_PROOF_MAGIC_VALUE: u32 = 13;

#[derive(Debug)]
pub struct ProofCommand {
    batches: Vec<SignedBatchEnvelope<FriProof>>,
    proof: SnarkProof,
}

impl ProofCommand {
    pub fn new(batches: Vec<SignedBatchEnvelope<FriProof>>, proof: SnarkProof) -> Self {
        assert!(
            !batches.is_empty(),
            "ProofCommand must contain at least one batch"
        );
        Self { batches, proof }
    }
}

impl SendToL1 for ProofCommand {
    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::L1SenderProve;
    const SENT_STAGE: BatchExecutionStage = BatchExecutionStage::ProveL1TxSent;
    const MINED_STAGE: BatchExecutionStage = BatchExecutionStage::ProveL1TxMined;
    const PASSTHROUGH_STAGE: BatchExecutionStage = BatchExecutionStage::ProveL1Passthrough;

    fn solidity_call(&self, _operator: &Address) -> Bytes {
        proveBatchesSharedBridgeCall::new((
            self.batches.first().unwrap().batch.chain_address,
            U256::from(self.batches.first().unwrap().batch_number()),
            U256::from(self.batches.last().unwrap().batch_number()),
            self.to_calldata_suffix().into(),
        ))
        .abi_encode()
        .into()
    }
}

impl AsRef<[SignedBatchEnvelope<FriProof>]> for ProofCommand {
    fn as_ref(&self) -> &[SignedBatchEnvelope<FriProof>] {
        self.batches.as_slice()
    }
}

impl AsMut<[SignedBatchEnvelope<FriProof>]> for ProofCommand {
    fn as_mut(&mut self) -> &mut [SignedBatchEnvelope<FriProof>] {
        self.batches.as_mut_slice()
    }
}

impl From<ProofCommand> for Vec<SignedBatchEnvelope<FriProof>> {
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

    /// `keccak(chainId | 0 | maxTxGasLimit)`, matching era-contracts#2323 and
    /// `zksync_os_native_pig::v32_chain_config_hash`. Middle word is
    /// `fri_proof_verification_enabled`, always disabled from L1.
    fn zksync_os_chain_config_hash(chain_id: u64) -> B256 {
        // EIP-7825 default cap (2^24), matching L1 and zk_ee.
        const DEFAULT_MAX_TX_GAS_LIMIT: u64 = 1 << 24;
        let mut bytes = Vec::with_capacity(32 * 3);
        bytes.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
        bytes.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        bytes.extend_from_slice(&U256::from(DEFAULT_MAX_TX_GAS_LIMIT).to_be_bytes::<32>());
        keccak256(&bytes)
    }

    /// v32 batch public input: chain config hash folded between the state commitments and the
    /// batch output hash (`batch.commitment`, chain-id-less for v32).
    fn get_batch_public_input_v32(
        prev_batch: &StoredBatchInfo,
        batch: &StoredBatchInfo,
        chain_config_hash: &B256,
    ) -> B256 {
        let mut bytes = Vec::with_capacity(32 * 4);
        bytes.extend_from_slice(prev_batch.state_commitment.as_slice());
        bytes.extend_from_slice(batch.state_commitment.as_slice());
        bytes.extend_from_slice(chain_config_hash.as_slice());
        bytes.extend_from_slice(batch.commitment.as_slice());
        keccak256(&bytes)
    }
    fn snark_public_input(
        previous_batch: &StoredBatchInfo,
        batches: &[StoredBatchInfo],
        chain_config_hash: Option<B256>,
    ) -> B256 {
        let mut hash_map: HashMap<usize, &StoredBatchInfo> = HashMap::new();
        hash_map.insert(previous_batch.batch_number as usize, previous_batch);
        for batch in batches {
            hash_map.insert(batch.batch_number as usize, batch);
        }
        let start = batches.first().unwrap().batch_number as usize;
        let end = batches.last().unwrap().batch_number as usize;

        // Pre-v32 folds a rolling chain of truncated hashes; v32 concatenates the full
        // per-batch hashes and hashes ONCE (a rolling fold coincides for N <= 2 but diverges
        // from N == 3 on). Single-batch ranges are the bare hash in both.
        let mut elements: Vec<B256> = Vec::with_capacity(end - start + 1);
        for i in start..=end {
            let batch = hash_map.get(&i).expect("Batch not found");
            let prev_batch = hash_map.get(&(i - 1)).expect("Previous batch not found");
            elements.push(match &chain_config_hash {
                Some(cch) => Self::get_batch_public_input_v32(prev_batch, batch, cch),
                None => Self::shift_b256_right(&Self::get_batch_public_input(prev_batch, batch)),
            });
        }

        if chain_config_hash.is_some() {
            let folded = if elements.len() == 1 {
                elements[0]
            } else {
                keccak256(elements.iter().flat_map(|e| e.0).collect::<Vec<u8>>())
            };
            Self::shift_b256_right(&folded)
        } else {
            // taken from https://github.com/mm-zk/zksync_tools/blob/cf2c47d61fa8399a030d0b31d4396832f802489b/prove_execute/src/main.rs
            let mut result: Option<B256> = None;
            for element in elements {
                match result {
                    Some(ref mut res) => {
                        let mut combined = [0_u8; 64];
                        combined[..32].copy_from_slice(&res.0);
                        combined[32..].copy_from_slice(&element.0);
                        *res = Self::shift_b256_right(&keccak256(combined));
                    }
                    None => result = Some(element),
                }
            }
            result.unwrap()
        }
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
            .map(|batch| batch.batch.batch_info.clone().into_stored())
            .collect();
        // todo: awful and temporary
        let verifier_version = match self.proof.proving_execution_version() {
            // Use default verifier for fake proofs.
            None => 0,
            Some(6) => 6,
            Some(7) => 0,
            // Switch to 0 once the L1 default verifier becomes the V8 one (as done for V7).
            Some(8) => 8,
            Some(execution_version) => panic!(
                "unsupported or old execution version: {execution_version}; there's no verifier defined for it"
            ),
        };

        // todo: remove tostring
        // v32.0 (proving V8) folds the chain config hash into the batch public input.
        let chain_config_hash = if self
            .batches
            .first()
            .unwrap()
            .batch
            .batch_info
            .protocol_version
            .minor
            >= 32
        {
            Some(Self::zksync_os_chain_config_hash(
                self.batches
                    .first()
                    .unwrap()
                    .batch
                    .batch_info
                    .commit_info
                    .chain_id,
            ))
        } else {
            None
        };
        let public_input =
            Self::snark_public_input(previous_batch_info, &stored_batch_infos, chain_config_hash);

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
            SnarkProof::Real(real) => {
                let proof: Vec<U256> = real
                    .proof()
                    .chunks(32)
                    .map(|chunk| {
                        let arr: [u8; 32] = chunk
                            .try_into()
                            .expect("proof bytes must be a multiple of 32");
                        U256::from_be_bytes(arr)
                    })
                    .collect();
                vec![
                    // Real proof versioned with a specific verifier
                    U256::from(OHBENDER_PROOF_TYPE | (verifier_version << 8)),
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

        /// Current commitment encoding version as per protocol.
        const SUPPORTED_ENCODING_VERSION: u8 = 1;

        let mut proof_data = vec![SUPPORTED_ENCODING_VERSION];
        proof_payload.abi_encode_raw(&mut proof_data);
        proof_data
    }
}
