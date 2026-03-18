use std::path::PathBuf;
use std::time::Duration;
use zksync_os_tx_validators::deployment_filter;
use zksync_os_types::NodeRole;

/// Configuration for all transaction validators applied during block production.
#[derive(Clone, Debug, Default)]
pub struct TxValidatorConfig {
    /// Deployment filter configuration.
    /// When enabled, only transactions from allowed deployers can deploy contracts.
    pub deployment_filter: deployment_filter::Config,
}

#[derive(Clone, Debug)]
pub struct SequencerConfig {
    /// Node's role in the network.
    pub node_role: NodeRole,

    /// Defines the block time for the sequencer.
    pub block_time: Duration,

    /// Max number of transactions in a block.
    pub max_transactions_in_block: usize,

    /// Path to the directory where block dumps for unexpected failures will be saved.
    pub block_dump_path: PathBuf,

    /// Max gas used per block
    pub block_gas_limit: u64,

    /// Max pubdata bytes per block
    pub block_pubdata_limit_bytes: u64,

    /// Maximum number of blocks to produce
    /// None for indefinite block production (normal operations)
    pub max_blocks_to_produce: Option<u64>,

    /// Max number of interop roots to be included in a single transaction
    pub interop_roots_per_tx: usize,

    /// Transaction validator configuration.
    pub tx_validator: TxValidatorConfig,
}
