use alloy::primitives::{
    Address, B256, Signature as AlloySignature, SignatureError, U256, keccak256,
};
use alloy::signers::Signer;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolValue, eip712_domain};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use zksync_os_contract_interface::calldata::encode_commit_batch_data;
use zksync_os_contract_interface::models::StoredBatchInfo;
use zksync_os_types::ProtocolSemanticVersion;

use crate::BatchInfo;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchSignatureSet(Vec<ValidatedBatchSignature>);

#[derive(Debug, thiserror::Error)]
pub enum BatchSignatureSetError {
    #[error("Duplicated signature")]
    DuplicatedSignature,
}

impl BatchSignatureSet {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        BatchSignatureSet(Vec::new())
    }

    pub fn push(
        &mut self,
        signature: ValidatedBatchSignature,
    ) -> Result<(), BatchSignatureSetError> {
        if self.0.contains(&signature) {
            return Err(BatchSignatureSetError::DuplicatedSignature);
        }
        self.0.push(signature);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn to_vec(&self) -> &Vec<ValidatedBatchSignature> {
        &self.0
    }

    /// Remove signatures not found on allowed list
    pub fn filter(mut self, allowed_signers: &[Address]) -> Self {
        self.0.retain(|s| allowed_signers.contains(&s.signer));
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BatchSignature(AlloySignature);

impl BatchSignature {
    /// Sign a batch for `commitBatchesMultisig`
    pub async fn sign_batch(
        prev_batch_info: &StoredBatchInfo,
        batch_info: &BatchInfo,
        l1_chain_id: u64,
        multisig_committer: Address,
        protocol_version: &ProtocolSemanticVersion,
        private_key: &PrivateKeySigner,
    ) -> Self {
        let digest = eip712_multisig_digest(
            prev_batch_info,
            batch_info,
            l1_chain_id,
            multisig_committer,
            protocol_version,
        );
        let signature = private_key.sign_hash(&digest).await.unwrap();
        BatchSignature(signature)
    }

    pub fn verify_signature(
        self,
        prev_batch_info: &StoredBatchInfo,
        batch_info: &BatchInfo,
        l1_chain_id: u64,
        multisig_committer: Address,
        protocol_version: &ProtocolSemanticVersion,
    ) -> Result<ValidatedBatchSignature, SignatureError> {
        Ok(ValidatedBatchSignature {
            signer: self
                .0
                .recover_address_from_prehash(&eip712_multisig_digest(
                    prev_batch_info,
                    batch_info,
                    l1_chain_id,
                    multisig_committer,
                    protocol_version,
                ))?,
            signature: self,
        })
    }
    pub fn into_raw(self) -> [u8; 65] {
        self.0.as_bytes()
    }

    pub fn from_raw_array(array: &[u8; 65]) -> Result<Self, SignatureError> {
        let signature = AlloySignature::from_raw_array(array)?;
        Ok(BatchSignature(signature))
    }
}

const TYPEHASH_BYTES: &[u8] = b"CommitBatchesMultisig(address chainAddress,uint256 processBatchFrom,uint256 processBatchTo,bytes batchData)";
static TYPEHASH: LazyLock<B256> = LazyLock::new(|| keccak256(TYPEHASH_BYTES));

/// Compute the full EIP-712 digest used by the `MultisigCommitter` contract
/// for the `commitBatchesMultisig` typed data, based on the given batch info
/// and L1 domain parameters.
fn eip712_multisig_digest(
    prev_batch_info: &StoredBatchInfo,
    batch_info: &BatchInfo,
    l1_chain_id: u64,
    multisig_committer: Address,
    protocol_version: &ProtocolSemanticVersion,
) -> B256 {
    let typehash = *TYPEHASH;

    let batch_data = encode_commit_batch_data(
        prev_batch_info,
        batch_info.commit_info.clone(),
        protocol_version.minor,
    );

    let batch_data_hash = keccak256(batch_data);

    let encoded = (
        typehash,
        batch_info.chain_address,
        U256::from(batch_info.batch_number), // processBatchFrom
        U256::from(batch_info.batch_number), // processBatchTo
        batch_data_hash,
    )
        .abi_encode_params();

    let struct_hash = keccak256(encoded);

    // Those name and version are currently used in contracts. This will need
    // to be updated if at some point we decide to change them
    let domain = eip712_domain! {
        name: "MultisigCommitter",
        version: "1",
        chain_id: l1_chain_id,
        verifying_contract: multisig_committer,
    };
    let domain_separator = domain.separator();

    keccak256(
        [
            &[0x19, 0x01],
            domain_separator.as_slice(),
            struct_hash.as_slice(),
        ]
        .concat(),
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatedBatchSignature {
    signature: BatchSignature,
    signer: Address,
}

impl ValidatedBatchSignature {
    pub fn signature(&self) -> &BatchSignature {
        &self.signature
    }

    pub fn signer(&self) -> &Address {
        &self.signer
    }
}

impl PartialEq for ValidatedBatchSignature {
    fn eq(&self, other: &Self) -> bool {
        self.signer == other.signer
    }
}
