use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct SequencerConfig {
    /// Defines the block time for the sequencer.
    pub block_time: Duration,

    /// Max number of transactions in a block.
    pub max_transactions_in_block: usize,

    /// Path to the directory where block dumps for unexpected failures will be saved.
    pub block_dump_path: PathBuf,

    /// Where to serve block replays
    pub block_replay_server_address: String,

    /// Where to download replays instead of actually running blocks.
    /// Setting this makes the node into an external node.
    pub block_replay_download_address: Option<String>,

    /// Max gas used per block
    pub block_gas_limit: u64,

    /// Max pubdata bytes per block
    pub block_pubdata_limit_bytes: u64,

    /// Maximum number of blocks to produce
    /// None for indefinite block production (normal operations)
    pub max_blocks_to_produce: Option<u64>,
}

impl SequencerConfig {
    pub fn is_main_node(&self) -> bool {
        self.block_replay_download_address.is_none()
    }

    pub fn is_external_node(&self) -> bool {
        !self.is_main_node()
    }
}
