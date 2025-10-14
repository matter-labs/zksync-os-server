use alloy::primitives::{Address, Signature as AlloySignature, SignatureError};
use alloy::signers::Signer;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolValue;
use serde::{Deserialize, Serialize};
use zksync_os_contract_interface::IExecutor::CommitBatchInfoZKsyncOS;
use zksync_os_contract_interface::models::CommitBatchInfo;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchSignatureSet(Vec<BatchSignature>);

impl BatchSignatureSet {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        BatchSignatureSet(Vec::new())
    }

    pub fn push(&mut self, signature: BatchSignature) {
        self.0.push(signature)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchSignature(AlloySignature);

impl BatchSignature {
    pub async fn sign_batch(batch_info: &CommitBatchInfo, private_key: &PrivateKeySigner) -> Self {
        let encoded = encode_batch_for_signing(batch_info);
        let signature = private_key.sign_message(&encoded).await.unwrap();
        BatchSignature(signature)
    }

    pub fn verify_signature(
        &self,
        batch_info: &CommitBatchInfo,
    ) -> Result<Address, SignatureError> {
        let encoded = encode_batch_for_signing(batch_info);
        self.0.recover_address_from_msg(encoded)
    }

    pub fn into_raw(self) -> [u8; 65] {
        self.0.as_bytes()
    }

    pub fn from_raw_array(array: &[u8; 65]) -> Result<Self, SignatureError> {
        let signature = AlloySignature::from_raw_array(array)?;
        Ok(BatchSignature(signature))
    }
}

fn encode_batch_for_signing(batch_info: &CommitBatchInfo) -> Vec<u8> {
    let alloy_batch_info = CommitBatchInfoZKsyncOS::from(batch_info.clone());
    alloy_batch_info.abi_encode_params()
}
