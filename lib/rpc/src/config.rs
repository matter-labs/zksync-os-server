use smart_config::metadata::TimeUnit;
use smart_config::{DescribeConfig, DeserializeConfig};
use std::time::Duration;

#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct RpcConfig {
    /// Gas limit of transactions executed via eth_call
    #[config(default_t = 10000000)]
    pub eth_call_gas: usize,

    /// Number of concurrent API connections (passed to jsonrpsee, default value there is 128)
    #[config(default_t = 1000)]
    pub max_connections: usize,

    /// Maximum RPC request payload size for both HTTP and WS in megabytes
    #[config(default_t = 15)]
    pub max_request_size: usize,

    /// Maximum RPC response payload size for both HTTP and WS in megabytes
    #[config(default_t = 24)]
    pub max_response_size: usize,

    /// Maximum number of blocks that could be scanned per filter
    #[config(default_t = 100_000)]
    pub max_blocks_per_filter: u64,

    /// Maximum number of logs that can be returned in a response
    #[config(default_t = 20_000)]
    pub max_logs_per_response: usize,

    /// Duration since the last filter poll, after which the filter is considered stale
    #[config(default_t = 15 * TimeUnit::Minutes)]
    pub stale_filter_ttl: Duration,
}

impl RpcConfig {
    /// Returns the max request size in bytes.
    pub fn max_request_size_bytes(&self) -> usize {
        self.max_request_size.saturating_mul(1024 * 1024)
    }

    /// Returns the max response size in bytes.
    pub fn max_response_size_bytes(&self) -> usize {
        self.max_response_size.saturating_mul(1024 * 1024)
    }
}
