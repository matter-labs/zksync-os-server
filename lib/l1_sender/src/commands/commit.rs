use crate::batcher_metrics::BatchExecutionStage;
use crate::batcher_model::{BatchEnvelope, FriProof};
use crate::commands::L1SenderCommand;
use crate::commands::tx_request_with_gas_fields;
use crate::commitment::PubdataDestination;
use crate::config::{BatchDaInputMode, L1SenderConfig};
use alloy::consensus::{Blob, BlobTransactionSidecar, Bytes48};
use alloy::network::{TransactionBuilder, TransactionBuilder4844};
use alloy::primitives::{Address, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::{SolCall, SolValue};
use std::fmt::Display;
use zksync_kzg::{KzgInfo, ZK_SYNC_BYTES_PER_BLOB};
use zksync_os_contract_interface::IExecutor;

#[derive(Debug)]
pub struct CommitCommand {
    input: BatchEnvelope<FriProof>,
    da_input_mode: BatchDaInputMode,
    pubdata_destination: PubdataDestination,
}

impl CommitCommand {
    pub fn new(
        input: BatchEnvelope<FriProof>,
        da_input_mode: BatchDaInputMode,
        pubdata_destination: PubdataDestination,
    ) -> Self {
        Self {
            input,
            da_input_mode,
            pubdata_destination,
        }
    }
}

impl L1SenderCommand for CommitCommand {
    const NAME: &'static str = "commit";
    const SENT_STAGE: BatchExecutionStage = BatchExecutionStage::CommitL1TxSent;
    const MINED_STAGE: BatchExecutionStage = BatchExecutionStage::CommitL1TxMined;
    fn solidity_call(&self) -> impl SolCall {
        IExecutor::commitBatchesSharedBridgeCall::new((
            self.input.batch.commit_batch_info.chain_address,
            U256::from(self.input.batch_number()),
            U256::from(self.input.batch_number()),
            self.to_calldata_suffix().into(),
        ))
    }

    async fn into_transaction_request(
        &self,
        provider: DynProvider,
        operator_address: Address,
        config: &L1SenderConfig<Self>,
        to_address: Address,
    ) -> anyhow::Result<TransactionRequest> {
        match self.pubdata_destination {
            PubdataDestination::Calldata => Ok(tx_request_with_gas_fields(
                &provider,
                operator_address,
                config.max_fee_per_gas(),
                config.max_priority_fee_per_gas(),
            )
            .await?
            .with_to(to_address)
            .with_call(&self.solidity_call())),
            PubdataDestination::Blobs => Ok(blob_tx_request_with_gas_fields(
                &provider,
                operator_address,
                config.max_fee_per_gas(),
                config.max_priority_fee_per_gas(),
                self.input.batch.commit_batch_info.pubdata.clone(),
            )
            .await?
            .with_to(to_address)
            .with_call(&self.solidity_call())),
        }
    }
}

async fn blob_tx_request_with_gas_fields(
    provider: &DynProvider,
    operator_address: Address,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    pubdata: Vec<u8>,
) -> anyhow::Result<TransactionRequest> {
    let eip1559_est = provider.estimate_eip1559_fees().await?;
    tracing::debug!(
        eip1559_est.max_priority_fee_per_gas,
        "estimated median priority fee (20% percentile) for the last 10 blocks"
    );
    if eip1559_est.max_fee_per_gas > max_fee_per_gas {
        tracing::warn!(
            max_fee_per_gas = max_fee_per_gas,
            estimated_max_fee_per_gas = eip1559_est.max_fee_per_gas,
            "L1 sender's configured maxFeePerGas is lower than the one estimated from network"
        );
    }
    if eip1559_est.max_priority_fee_per_gas > max_priority_fee_per_gas {
        tracing::warn!(
            max_priority_fee_per_gas = max_priority_fee_per_gas,
            estimated_max_priority_fee_per_gas = eip1559_est.max_priority_fee_per_gas,
            "L1 sender's configured maxPriorityFeePerGas is lower than the one estimated from network"
        );
    }

    let blob_fee = provider.get_blob_base_fee().await.unwrap();

    let (blobs, commitments, proofs) = pubdata
        .chunks(ZK_SYNC_BYTES_PER_BLOB)
        .map(|blob| {
            let kzg_info = KzgInfo::new(blob);
            let blob: Blob = Blob::from_slice(&kzg_info.blob);
            let commitment = Bytes48::new(kzg_info.kzg_commitment);
            let proof = Bytes48::new(kzg_info.blob_proof);
            (blob, commitment, proof)
        })
        .fold(
            (vec![], vec![], vec![]),
            |(mut blobs, mut commitments, mut proofs), (blob, commitment, proof)| {
                blobs.push(blob);
                commitments.push(commitment);
                proofs.push(proof);
                (blobs, commitments, proofs)
            },
        );

    let sidecar = BlobTransactionSidecar {
        blobs,
        commitments,
        proofs,
    };

    let tx = TransactionRequest::default()
        .with_from(operator_address)
        .with_max_fee_per_gas(max_fee_per_gas)
        .with_max_priority_fee_per_gas(max_priority_fee_per_gas)
        // Default value for `max_aggregated_tx_gas` from zksync-era, should always be enough
        .with_gas_limit(15000000)
        .with_max_fee_per_blob_gas(blob_fee)
        .with_blob_sidecar(sidecar);
    Ok(tx)
}

impl AsRef<[BatchEnvelope<FriProof>]> for CommitCommand {
    fn as_ref(&self) -> &[BatchEnvelope<FriProof>] {
        std::slice::from_ref(&self.input)
    }
}

impl AsMut<[BatchEnvelope<FriProof>]> for CommitCommand {
    fn as_mut(&mut self) -> &mut [BatchEnvelope<FriProof>] {
        std::slice::from_mut(&mut self.input)
    }
}

impl From<CommitCommand> for Vec<BatchEnvelope<FriProof>> {
    fn from(value: CommitCommand) -> Self {
        vec![value.input]
    }
}

impl Display for CommitCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "commit batch {}", self.input.batch_number())?;
        Ok(())
    }
}

impl CommitCommand {
    /// `commitBatchesSharedBridge` expects the rest of calldata to be of very specific form. This
    /// function makes sure last committed batch and new batch are encoded correctly.
    fn to_calldata_suffix(&self) -> Vec<u8> {
        /// Current commitment encoding version for ZKsync OS.
        const SUPPORTED_ENCODING_VERSION: u8 = 2;

        let stored_batch_info =
            IExecutor::StoredBatchInfo::from(&self.input.batch.previous_stored_batch_info);
        let commit_batch_info = self
            .input
            .batch
            .commit_batch_info
            .clone()
            .into_l1_commit_data(self.da_input_mode);
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
