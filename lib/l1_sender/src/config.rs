use alloy::consensus::constants::GWEI_TO_WEI;
use serde::{Deserialize, Serialize};
use smart_config::value::SecretString;
use std::time::Duration;

/// Configuration of L1 sender.
#[derive(Clone, Debug)]
pub struct L1SenderConfig {
    /// Private key to operate from.
    /// Depending on the mode, this can be a commit/prove/execute operator.
    pub operator_pk: SecretString,

    /// Max fee per gas we are willing to spend (in gwei).
    pub max_fee_per_gas_gwei: u64,

    /// Max priority fee per gas we are willing to spend (in gwei).
    pub max_priority_fee_per_gas_gwei: u64,

    /// Max number of commands (to commit/prove/execute one batch) to be processed at a time.
    pub command_limit: usize,

    /// How often to poll L1 for new blocks.
    pub poll_interval: Duration,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum BatchDaInputMode {
    Rollup,
    Validium,
}

impl L1SenderConfig {
    /// Max fee per gas we are willing to spend (in wei).
    pub fn max_fee_per_gas(&self) -> u128 {
        self.max_fee_per_gas_gwei as u128 * (GWEI_TO_WEI as u128)
    }

    /// Max priority fee per gas we are willing to spend (in wei).
    pub fn max_priority_fee_per_gas(&self) -> u128 {
        self.max_priority_fee_per_gas_gwei as u128 * (GWEI_TO_WEI as u128)
    }
}
