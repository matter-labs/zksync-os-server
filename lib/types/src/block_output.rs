use alloy::consensus::{Header, Sealed};
use alloy::primitives::{B256, keccak256};
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::types::{AccountDiff, StorageWrite, TxOutput};

#[derive(Debug, Clone)]
pub struct BlockOutput {
    pub header: Sealed<Header>,
    pub tx_results: Vec<Result<TxOutput, InvalidTransaction>>,
    pub storage_writes: Vec<StorageWrite>,
    pub account_diffs: Vec<AccountDiff>,
    pub published_preimages: Vec<(B256, Vec<u8>)>,
    pub pubdata: Vec<u8>,
    pub computational_native_used: u64,
}

/// Hash committing to a block's execution outcome, used to detect divergent execution:
/// two nodes that executed the same block must arrive at the identical hash.
///
/// Deliberately incomplete — it covers the pieces that are most likely to differ when
/// execution diverges (header hash, per-transaction status and gas, storage writes)
/// rather than every field of the output.
pub fn hash_block_output(block_output: &BlockOutput) -> B256 {
    let mut preimage = Vec::new();
    preimage.extend_from_slice(block_output.header.hash().as_slice());
    for tx in block_output.tx_results.iter().flatten() {
        preimage.extend_from_slice(&[tx.is_success() as u8]);
        preimage.extend_from_slice(&tx.gas_used.to_be_bytes());
    }
    for storage_log in &block_output.storage_writes {
        preimage.extend_from_slice(storage_log.key.as_slice());
        preimage.extend_from_slice(storage_log.value.as_slice());
    }

    keccak256(preimage)
}
