use crate::batcher_metrics::BatchExecutionStage;
use crate::batcher_model::{BatchSignatureData, FriProof, SignedBatchEnvelope};
use crate::commands::SendToL1;
use alloy::consensus::BlobTransactionSidecar;
use alloy::primitives::{Bytes, U256};
use alloy::sol_types::{SolCall, SolValue};
use std::fmt::Display;
use zksync_os_batch_types::BatchSignatureSet;
use zksync_os_contract_interface::l1_discovery::BatchVerificationL1;
use zksync_os_contract_interface::{IExecutor, IExecutorV29, IMultisigCommitter};

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
        l1_config: &BatchVerificationL1,
        input: SignedBatchEnvelope<FriProof>,
    ) -> Result<Self, BatchVerificationError> {
        match (l1_config, input.signature_data.clone()) {
            (BatchVerificationL1::Disabled, _) => Ok(Self {
                input,
                signatures: None,
            }),
            (
                BatchVerificationL1::Enabled(l1_config),
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
            (BatchVerificationL1::Enabled(_), _) => Err(BatchVerificationError::BatchNotSigned),
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

    fn solidity_call(&self) -> Bytes {
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
                self.to_calldata_suffix().into(),
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
                self.to_calldata_suffix().into(),
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

impl CommitCommand {
    /// `commitBatchesSharedBridge` expects the rest of calldata to be of very specific form. This
    /// function makes sure last committed batch and new batch are encoded correctly.
    pub(super) fn to_calldata_suffix(&self) -> Vec<u8> {
        let stored_batch_info =
            IExecutor::StoredBatchInfo::from(&self.input.batch.previous_stored_batch_info);

        match self.input.batch.protocol_version.minor {
            29 => {
                const V29_ENCODING_VERSION: u8 = 2;

                let commit_batch_info = IExecutorV29::CommitBatchInfoZKsyncOS::from(
                    self.input.batch.batch_info.commit_info.clone(),
                );
                tracing::debug!(
                    last_batch_hash = ?self.input.batch.previous_stored_batch_info.hash(),
                    last_batch_number = ?self.input.batch.previous_stored_batch_info.batch_number,
                    new_batch_number = ?commit_batch_info.batchNumber,
                    "preparing commit calldata"
                );
                let encoded_data = (stored_batch_info, vec![commit_batch_info]).abi_encode_params();

                // Prefixed by current encoding version as expected by protocol
                [[V29_ENCODING_VERSION].to_vec(), encoded_data].concat()
            }
            // 31 needed for upgrade integration test
            30 | 31 => {
                const V30_ENCODING_VERSION: u8 = 3;

                let commit_batch_info = IExecutor::CommitBatchInfoZKsyncOS::from(
                    self.input.batch.batch_info.commit_info.clone(),
                );
                tracing::debug!(
                    last_batch_hash = ?self.input.batch.previous_stored_batch_info.hash(),
                    last_batch_number = ?self.input.batch.previous_stored_batch_info.batch_number,
                    new_batch_number = ?commit_batch_info.batchNumber,
                    "preparing commit calldata"
                );
                let encoded_data = (stored_batch_info, vec![commit_batch_info]).abi_encode_params();

                // Prefixed by current encoding version as expected by protocol
                [[V30_ENCODING_VERSION].to_vec(), encoded_data].concat()
            }
            _ => panic!(
                "Unsupported protocol version: {}",
                self.input.batch.protocol_version
            ),
        }
    }
}
