use alloy::primitives::{B256, keccak256};
use zk_os_forward_system::run::BlockOutput;

pub(crate) fn hash_block_output(block_output: &BlockOutput) -> B256 {
    let mut preimage = Vec::new();
    preimage.extend_from_slice(&block_output.header.hash());
    for tx in block_output.tx_results.iter().flatten() {
        preimage.extend_from_slice(&[tx.is_success() as u8]);
        preimage.extend_from_slice(&tx.gas_used.to_be_bytes());
    }
    for storage_log in &block_output.storage_writes {
        preimage.extend_from_slice(storage_log.key.as_u8_array_ref());
        preimage.extend_from_slice(storage_log.value.as_u8_array_ref());
    }

    keccak256(preimage)
}
