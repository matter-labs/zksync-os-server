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
const MULTI_PROOF_TYPE: u32 = 5;

#[derive(Debug)]
pub struct ProofCommand {
    batches: Vec<SignedBatchEnvelope<FriProof>>,
    proof: SnarkProof,
}

/// Errors from proof calldata encoding.
#[derive(Debug, thiserror::Error)]
pub enum ProofEncodingError {
    #[error("Airbender proof length ({len}) is not a multiple of 32")]
    AirbenderProofNotAligned { len: usize },
    #[error("unsupported execution version: {version}")]
    UnsupportedExecutionVersion { version: u32 },
}

impl ProofCommand {
    pub fn new(batches: Vec<SignedBatchEnvelope<FriProof>>, proof: SnarkProof) -> Self {
        assert!(
            !batches.is_empty(),
            "ProofCommand must contain at least one batch"
        );
        Self { batches, proof }
    }

    /// Decompose into parts. Used for error recovery when downstream send fails.
    pub fn batches(&self) -> &[SignedBatchEnvelope<FriProof>] {
        &self.batches
    }

    pub fn proof(&self) -> &SnarkProof {
        &self.proof
    }

    pub fn into_parts(self) -> (Vec<SignedBatchEnvelope<FriProof>>, SnarkProof) {
        (self.batches, self.proof)
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
        self.try_to_calldata_suffix()
            .expect("proof calldata encoding failed: this is a critical pipeline bug")
    }

    fn try_to_calldata_suffix(&self) -> Result<Vec<u8>, ProofEncodingError> {
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
        // A MultiProof routes its execution version straight through. The
        // on-chain MultiProofVerifier resolves the sub-verifiers from that
        // version, so the mapping below applies to the other proof kinds only.
        let verifier_version = if let SnarkProof::MultiProof(multi_proof) = &self.proof {
            multi_proof.proving_execution_version()
        } else {
            // todo: awful and temporary
            match self.proof.proving_execution_version() {
                // Use default verifier for fake proofs.
                None => 0,
                Some(6) => 6,
                Some(7) => 0,
                // Switch to 0 once the L1 default verifier becomes the V8 one (as done for V7).
                Some(8) => 8,
                Some(version) => {
                    return Err(ProofEncodingError::UnsupportedExecutionVersion { version });
                }
            }
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
            SnarkProof::MultiProof(multi_proof) => {
                // The two shapes are invariants of the on-chain verifiers and
                // are checked when the proofs are composed, so there is nothing
                // to validate here.
                // The type-5 payload carries the SNARK words only. The on-chain
                // MultiProofVerifier reconstructs the ZiSK public values from its
                // pinned VKs and the batch public inputs. The aggregation manager
                // validates the binding digest off-chain before submission.
                let to_u256_chunks = |bytes: &[u8]| -> Vec<U256> {
                    bytes
                        .chunks_exact(32)
                        .map(|c| {
                            let arr: [u8; 32] = c.try_into().unwrap();
                            U256::from_be_bytes(arr)
                        })
                        .collect()
                };

                let airbender_chunks = to_u256_chunks(multi_proof.airbender_proof());
                let zisk_proof_chunks = to_u256_chunks(multi_proof.zisk_proof());

                let mut proof_vec = vec![
                    U256::from(MULTI_PROOF_TYPE | (verifier_version << 8)),
                    U256::from(0),                      // previous hash
                    U256::from(airbender_chunks.len()), // N
                ];
                proof_vec.extend(airbender_chunks);
                proof_vec.extend(zisk_proof_chunks);
                proof_vec
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
        Ok(proof_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zksync_os_batch_types::batcher_model::{MultiProofSnarkProof, ZISK_SNARK_PROOF_BYTES};

    #[test]
    fn test_multi_proof_serde_roundtrip() {
        let multi_proof =
            MultiProofSnarkProof::new(vec![0xAB; 64], vec![0xCD; 768], 6).expect("well-shaped");
        let snark = SnarkProof::MultiProof(multi_proof);
        let json = serde_json::to_string(&snark).unwrap();
        let decoded: SnarkProof = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.proving_execution_version(), Some(6));
        assert_eq!(decoded.airbender_proof().unwrap().len(), 64);
    }

    #[test]
    fn test_multi_proof_proving_version() {
        let snark = SnarkProof::MultiProof(
            MultiProofSnarkProof::new(vec![], vec![0; ZISK_SNARK_PROOF_BYTES], 6)
                .expect("well-shaped"),
        );
        assert_eq!(snark.proving_execution_version(), Some(6));

        // A MultiProof carries two proofs; `airbender_proof` names which one.
        let snark2 = SnarkProof::MultiProof(
            MultiProofSnarkProof::new(vec![7; 32], vec![0; ZISK_SNARK_PROOF_BYTES], 5)
                .expect("well-shaped"),
        );
        assert_eq!(snark2.airbender_proof(), Some(&[7u8; 32][..]));
        assert_eq!(snark2.proving_execution_version(), Some(5));

        // The shapes the on-chain verifiers require are checked here, not at
        // the encoder.
        assert!(MultiProofSnarkProof::new(vec![], vec![0; 10], 6).is_err());
        assert!(
            MultiProofSnarkProof::new(vec![0; 33], vec![0; ZISK_SNARK_PROOF_BYTES], 6).is_err()
        );
    }

    #[test]
    fn test_backward_compat_existing_variants() {
        // Fake proof still works
        let fake = SnarkProof::Fake;
        assert_eq!(fake.proving_execution_version(), None);
        assert!(fake.airbender_proof().is_none());

        // Real proof still works
        let real = SnarkProof::Real(zksync_os_batch_types::batcher_model::RealSnarkProof::V2 {
            proof: vec![0xAA; 32],
            proving_execution_version: 6,
        });
        assert_eq!(real.proving_execution_version(), Some(6));
        assert_eq!(real.airbender_proof().unwrap().len(), 32);
    }

    /// The type promises its two shapes, and `serde` must not be able to hand
    /// back a value that breaks them: `derive(Deserialize)` would write the
    /// private fields directly, so deserialization goes through the
    /// constructor.
    #[test]
    fn deserialization_cannot_bypass_the_shape_checks() {
        let good = SnarkProof::MultiProof(
            MultiProofSnarkProof::new(vec![0xAB; 64], vec![0xCD; ZISK_SNARK_PROOF_BYTES], 6)
                .expect("well-shaped"),
        );
        let json = serde_json::to_string(&good).unwrap();
        serde_json::from_str::<SnarkProof>(&json).expect("a valid value round-trips");

        // A ZiSK proof one byte short, and an unaligned Airbender proof.
        let short = json.replace(
            &format!(
                "\"zisk_proof\":[{}]",
                vec!["205"; ZISK_SNARK_PROOF_BYTES].join(",")
            ),
            &format!(
                "\"zisk_proof\":[{}]",
                vec!["205"; ZISK_SNARK_PROOF_BYTES - 1].join(",")
            ),
        );
        assert_ne!(short, json, "the fixture must actually shorten the proof");
        assert!(
            serde_json::from_str::<SnarkProof>(&short).is_err(),
            "a short ZiSK proof must not deserialize"
        );
        let unaligned = json.replace(
            &format!("\"airbender_proof\":[{}]", vec!["171"; 64].join(",")),
            &format!("\"airbender_proof\":[{}]", vec!["171"; 63].join(",")),
        );
        assert_ne!(
            unaligned, json,
            "the fixture must actually unalign the proof"
        );
        assert!(
            serde_json::from_str::<SnarkProof>(&unaligned).is_err(),
            "an unaligned Airbender proof must not deserialize"
        );
    }
}
