use std::time::Duration;

#[derive(Clone, Debug)]
pub struct RpcConfig {
    /// Gas limit of transactions executed via eth_call
    pub eth_call_gas: usize,

    /// Number of concurrent API connections (passed to jsonrpsee, default value there is 128)
    pub max_connections: u32,

    /// Maximum RPC request payload size for both HTTP and WS in megabytes
    pub max_request_size: u32,

    /// Maximum RPC response payload size for both HTTP and WS in megabytes
    pub max_response_size: u32,

    /// Maximum number of blocks that could be scanned per filter
    pub max_blocks_per_filter: u64,

    /// Maximum number of logs that can be returned in a response
    pub max_logs_per_response: usize,

    /// Duration since the last filter poll, after which the filter is considered stale
    pub stale_filter_ttl: Duration,
}

impl RpcConfig {
    /// Returns the max request size in bytes.
    pub fn max_request_size_bytes(&self) -> u32 {
        self.max_request_size.saturating_mul(1024 * 1024)
    }

    /// Returns the max response size in bytes.
    pub fn max_response_size_bytes(&self) -> u32 {
        self.max_response_size.saturating_mul(1024 * 1024)
    }
}
