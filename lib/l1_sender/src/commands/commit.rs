use crate::batcher_metrics::BatchExecutionStage;
use crate::batcher_model::{BatchSignatureData, FriProof, SignedBatchEnvelope};
use crate::commands::SendToL1;
use alloy::consensus::BlobTransactionSidecar;
use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;
use std::fmt::Display;
use zksync_os_batch_types::BatchSignatureSet;
use zksync_os_contract_interface::calldata::encode_commit_batch_data;
use zksync_os_contract_interface::l1_discovery::BatchVerificationSL;
use zksync_os_contract_interface::{IExecutor, IMultisigCommitter};

#[derive(Debug)]
pub struct CommitCommand {
    pub(super) input: SignedBatchEnvelope<FriProof>,
    pub(super) signatures: Option<BatchSignatureSet>,
}

#[derive(Debug, thiserror::Error)]
pub enum BatchVerificationError {
    #[error("Batch was not signed")]
    BatchNotSigned,
    #[error("Not enough signatures, we have {} but need {}", .0, .1)]
    NotEnoughSignatures(u64, u64),
}

impl CommitCommand {
    /// This function should not error normally, however if the signatures
    /// attached to batch do not allow for submission to L1 it will error
    /// instead of causing a reverted transaction.
    pub fn try_new(
        l1_config: &BatchVerificationSL,
        input: SignedBatchEnvelope<FriProof>,
    ) -> Result<Self, BatchVerificationError> {
        match (l1_config, input.signature_data.clone()) {
            (BatchVerificationSL::Disabled, _) => Ok(Self {
                input,
                signatures: None,
            }),
            (
                BatchVerificationSL::Enabled(l1_config),
                BatchSignatureData::Signed { signatures },
            ) => {
                let allowed_signers = &l1_config.validators;
                let filtered_signatures = signatures.filter(allowed_signers);
                // edge case: if threshold is 0 it is safe to submit 0 signatures
                if u64::try_from(filtered_signatures.len()).unwrap() < l1_config.threshold {
                    return Err(BatchVerificationError::NotEnoughSignatures(
                        u64::try_from(filtered_signatures.len()).unwrap(), //its fairly safe to convert usize into u64
                        l1_config.threshold,
                    ));
                }
                Ok(Self {
                    input,
                    signatures: Some(filtered_signatures),
                })
            }
            (BatchVerificationSL::Enabled(l1_config), _) => {
                // actually if threshold is 0 its still ok without signing enabled
                if l1_config.threshold == 0 {
                    Ok(Self {
                        input,
                        signatures: None,
                    })
                } else {
                    Err(BatchVerificationError::BatchNotSigned)
                }
            }
        }
    }

    pub(crate) fn input(&self) -> &SignedBatchEnvelope<FriProof> {
        &self.input
    }
}

impl SendToL1 for CommitCommand {
    const NAME: &'static str = "commit";
    const SENT_STAGE: BatchExecutionStage = BatchExecutionStage::CommitL1TxSent;
    const MINED_STAGE: BatchExecutionStage = BatchExecutionStage::CommitL1TxMined;
    const PASSTHROUGH_STAGE: BatchExecutionStage = BatchExecutionStage::CommitL1Passthrough;

    fn solidity_call(&self, _gateway: bool, _operator: &Address) -> Bytes {
        if let Some(signatures_set) = &self.signatures {
            let mut signatures = signatures_set.to_vec().clone();
            signatures.sort_by(|a, b| a.signer().cmp(b.signer()));
            let (signers, signatures): (Vec<_>, Vec<Bytes>) = signatures
                .into_iter()
                .map(|s| {
                    let signer = *s.signer();
                    let signature_bytes: Bytes = s.signature().clone().into_raw().to_vec().into();
                    (signer, signature_bytes)
                })
                .unzip();

            IMultisigCommitter::commitBatchesMultisigCall::new((
                self.input.batch.batch_info.chain_address,
                U256::from(self.input.batch_number()),
                U256::from(self.input.batch_number()),
                encode_commit_batch_data(
                    &self.input.batch.previous_stored_batch_info,
                    self.input.batch.batch_info.commit_info.clone(),
                    self.input.batch.protocol_version.minor,
                )
                .into(),
                signers,
                signatures,
            ))
            .abi_encode()
            .into()
        } else {
            // todo: encode through `CommitCalldata` instead
            IExecutor::commitBatchesSharedBridgeCall::new((
                self.input.batch.batch_info.chain_address,
                U256::from(self.input.batch_number()),
                U256::from(self.input.batch_number()),
                encode_commit_batch_data(
                    &self.input.batch.previous_stored_batch_info,
                    self.input.batch.batch_info.commit_info.clone(),
                    self.input.batch.protocol_version.minor,
                )
                .into(),
            ))
            .abi_encode()
            .into()
        }
    }

    fn blob_sidecar(&self) -> Option<BlobTransactionSidecar> {
        self.input.batch.batch_info.blob_sidecar.clone()
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
        if let Some(signatures_set) = &self.signatures {
            write!(
                f,
                "signed commit batch {}, signatures: {}",
                self.input.batch_number(),
                signatures_set
                    .to_vec()
                    .iter()
                    .map(|s| s.signer().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            )?;
        } else {
            write!(f, "commit batch {}", self.input.batch_number())?;
        }
        Ok(())
    }
}
