use alloy::primitives::{B256, keccak256};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use zksync_os_interface::common_types::{BlockContext, BlockOutput};
use zksync_os_types::ZkTransaction;

// Hash of the block output, which is used to identify divergences in block execution.
// It's incomplete, in a sense that it does not include all the data from the block output.
// Hash includes the most important pieces of data that are likely to change in case of a divergence.
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

#[derive(Debug, Serialize, Deserialize)]
pub struct BlockDump {
    pub ctx: BlockContext,
    pub txs: Vec<ZkTransaction>,
    pub error: String,
}

pub(crate) fn save_dump(path: PathBuf, dump: BlockDump) -> anyhow::Result<()> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Incorrect system time")
        .as_secs();
    let file_name = format!("dump_{}_{seconds}.json", dump.ctx.block_number);
    let bytes = serde_json::to_vec(&dump).context("failed to serialize dump")?;
    std::fs::create_dir_all(&path).context("create_dir_all")?;
    std::fs::write(path.join(file_name), bytes).context("failed to write dump file")?;

    Ok(())
}
