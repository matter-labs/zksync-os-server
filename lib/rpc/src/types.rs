/// Limits for logs queries.
/// Copied from reth: `crates/rpc/rpc-eth-api/src/filter.rs`,
/// consider importing directly if `reth:rpc` crate is added as a dependency.
#[derive(Default, Debug, Clone, Copy)]
pub struct QueryLimits {
    /// Maximum number of blocks that could be scanned per filter
    pub max_blocks_per_filter: Option<u64>,
    /// Maximum number of logs that can be returned in a response
    pub max_logs_per_response: Option<usize>,
}

impl QueryLimits {
    pub fn new(max_blocks_per_filter: u64, max_logs_per_response: usize) -> Self {
        Self {
            max_blocks_per_filter: Some(max_blocks_per_filter),
            max_logs_per_response: Some(max_logs_per_response),
        }
    }
}
