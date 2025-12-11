use std::fmt::Display;

use alloy::{
    consensus::BlobTransactionSidecar,
    primitives::{Bytes, U256},
    sol_types::SolCall,
};
use zksync_os_batch_types::BatchSignatureSet;
use zksync_os_contract_interface::IMultisigCommitter;

use super::commit::CommitCommand;
use crate::{
    batcher_metrics::BatchExecutionStage,
    batcher_model::{BatchSignatureData, FriProof, SignedBatchEnvelope},
    commands::SendToL1,
};

pub struct SignedCommitCommand {
    inner: CommitCommand,
    signatures: BatchSignatureSet,
}

#[derive(Debug, thiserror::Error)]
pub enum BatchVerificationError {
    #[error("Batch was not signed")]
    BatchNotSigned,
}

impl SignedCommitCommand {
    pub fn try_new(input: SignedBatchEnvelope<FriProof>) -> Result<Self, BatchVerificationError> {
        let signature_data = input.signature_data.clone();
        match signature_data {
            BatchSignatureData::Signed { signatures } => Ok(Self {
                inner: CommitCommand::new(input),
                signatures,
            }),
            _ => Err(BatchVerificationError::BatchNotSigned),
        }
    }
}

impl SendToL1 for SignedCommitCommand {
    const NAME: &'static str = "signedCommit";
    const SENT_STAGE: BatchExecutionStage = BatchExecutionStage::CommitL1TxSent;
    const MINED_STAGE: BatchExecutionStage = BatchExecutionStage::CommitL1TxMined;
    const PASSTHROUGH_STAGE: BatchExecutionStage = BatchExecutionStage::CommitL1Passthrough;

    fn solidity_call(&self) -> impl SolCall {
        let mut signatures = self.signatures.to_vec().clone();
        signatures.sort_by(|a, b| a.signer().cmp(b.signer()));
        let (signers, signatures) = signatures
            .into_iter()
            .map(|s| {
                let signer = *s.signer();
                let signature_bytes: Bytes = s.signature().clone().into_raw().to_vec().into();
                (signer, signature_bytes)
            })
            .unzip();

        IMultisigCommitter::commitBatchesMultisigCall::new((
            self.inner.input.batch.batch_info.chain_address,
            U256::from(self.inner.input.batch_number()),
            U256::from(self.inner.input.batch_number()),
            self.inner.to_calldata_suffix().into(),
            signers,
            signatures,
        ))
    }

    fn blob_sidecar(&self) -> Option<BlobTransactionSidecar> {
        self.inner.blob_sidecar()
    }
}

impl AsRef<[SignedBatchEnvelope<FriProof>]> for SignedCommitCommand {
    fn as_ref(&self) -> &[SignedBatchEnvelope<FriProof>] {
        self.inner.as_ref()
    }
}

impl AsMut<[SignedBatchEnvelope<FriProof>]> for SignedCommitCommand {
    fn as_mut(&mut self) -> &mut [SignedBatchEnvelope<FriProof>] {
        self.inner.as_mut()
    }
}

impl From<SignedCommitCommand> for Vec<SignedBatchEnvelope<FriProof>> {
    fn from(value: SignedCommitCommand) -> Self {
        value.inner.into()
    }
}

impl Display for SignedCommitCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "signed commit batch {}, signatures: {}",
            self.inner.input.batch_number(),
            self.signatures
                .to_vec()
                .iter()
                .map(|s| s.signer().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        Ok(())
    }
}
