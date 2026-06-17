use crate::limits::Limits;
use crate::method_filter::MethodFilter;
use alloy::primitives::Address;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Duration;

/// How the JSON-RPC server throttles incoming requests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum RateLimitPolicy {
    /// No rate limiting.
    #[default]
    Disabled,
    /// Hardcoded per-node buckets. Scale capacity by adding nodes behind a load balancer.
    Tiered,
}

impl RateLimitPolicy {
    pub(crate) fn to_limits(self) -> Limits {
        match self {
            Self::Disabled => Limits::default(),
            Self::Tiered => Limits::tiered(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RpcConfig {
    /// JSON-RPC address to listen on. Only http is currently supported.
    pub address: String,

    /// Gas limit of transactions executed via eth_call
    pub eth_call_gas: usize,

    /// Maximum block gas limit accepted for an `eth_simulateV1` block override. Applies only
    /// when the caller explicitly overrides `blockOverrides.gasLimit`; unset overrides fall
    /// back to the executing block's own gas limit.
    pub eth_simulate_block_gas_limit: u64,

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

    /// List of L2 signer addresses to blacklist (i.e. their transactions are rejected).
    pub l2_signer_blacklist: HashSet<Address>,

    /// Default timeout for `eth_sendRawTransactionSync`
    pub send_raw_transaction_sync_timeout: Duration,

    /// Factor applied to the pending block base fee returned by `eth_gasPrice`.
    pub gas_price_scale_factor: f64,

    /// Factor for pubdata price used during gas limit estimation (`eth_estimateGas`).
    /// Needed to account for pubdata price market fluctuations. Setting this to `1.0` can lead to
    /// users submitting unexecutable transactions (fail with `OutOfNativeResourcesDuringValidation`)
    /// because pubdata price increase in-between estimation and sequencing.
    pub estimate_gas_pubdata_price_factor: f64,

    /// Rate-limiting policy for incoming requests.
    pub rate_limit_policy: RateLimitPolicy,

    /// Method-availability filter: which RPC methods the server will respond to.
    pub method_filter: MethodFilter,
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
