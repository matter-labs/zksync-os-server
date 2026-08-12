use alloy::primitives::Address;
use std::collections::HashSet;

pub struct TxValidatorConfig {
    /// Max input size of a transaction to be accepted by mempool
    pub max_input_bytes: usize,
}

/// Configuration for the executed-gas transaction rate limiter: gates admission of new L2
/// transactions based on the sequencer's *total* recent execution throughput — L1 priority,
/// upgrade, and interop transactions all count toward it too, even though only L2 admission
/// is ever actually gated.
/// See [`crate::subpools::rate_limited_l2::TxGasRateLimiter`] for the mechanics.
#[derive(Clone, Debug)]
pub struct TxGasRateLimitConfig {
    /// Target sustained executed-gas throughput, gas per second.
    pub gas_per_second: u64,
    /// Bank capacity (idle burst headroom), in seconds' worth of `gas_per_second`.
    pub max_credit_seconds: f64,
    /// Credit required to reopen the gate, in seconds' worth of `gas_per_second`.
    pub reopen_credit_seconds: f64,
    /// Max remembered deficit, in seconds' worth of `gas_per_second`. `0` clamps the bank at zero.
    pub deficit_floor_seconds: f64,
    /// Senders whose transactions are never rate-limited.
    pub exempt_senders: HashSet<Address>,
}
