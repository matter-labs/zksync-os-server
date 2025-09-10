use smart_config::{DescribeConfig, DeserializeConfig};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct SequencerConfig {
    /// Defines the block time for the sequencer.
    #[config(default_t = Duration::from_millis(100))]
    pub block_time: Duration,

    /// Max number of transactions in a block.
    #[config(default_t = 1000)]
    pub max_transactions_in_block: usize,

    /// Path to the directory where block dumps for unexpected failures will be saved.
    #[config(default_t = "./db/block_dumps".into())]
    pub block_dump_path: PathBuf,

    /// Where to download replays instead of actually running blocks.
    /// Setting this makes the node into an external node.
    #[config(default_t = None)]
    pub block_replay_download_address: Option<String>,

    /// Max gas used per block
    #[config(default_t = 100_000_000)]
    pub block_gas_limit: u64,

    /// Max pubdata bytes per block
    #[config(default_t = 110_000)]
    pub block_pubdata_limit_bytes: u64,
}

impl SequencerConfig {
    pub fn is_main_node(&self) -> bool {
        self.block_replay_download_address.is_none()
    }
}
