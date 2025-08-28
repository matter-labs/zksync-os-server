use alloy::consensus::constants::GWEI_TO_WEI;
use serde::{Deserialize, Serialize};
use smart_config::value::SecretString;
use smart_config::{DescribeConfig, DeserializeConfig};
use std::time::Duration;

/// Configuration of L1 sender.
/// todo: consider renaming to L1Config and using in L1Watcher as well.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct L1SenderConfig {
    /// Private key to commit batches to L1
    /// Must be consistent with the operator key set on the contract (permissioned!)
    // TODO: Pre-configured value, to be removed
    #[config(alias = "operator_private_key", default_t = "0x77e0fabff0a8d372eb1c26eb8375345faa1f794d6a4ca833a6f7bba106d53938".into())]
    pub operator_commit_pk: SecretString,

    /// Private key to use to submit proofs to L1
    /// Can be arbitrary funded address - proof submission is permissionless.
    // TODO: Pre-configured value, to be removed
    #[config(default_t = "0x3c5722e7d93b0f5c3ce3a743f283c1958acbfc1d79e32823c6f9cc1cd6523429".into())]
    pub operator_prove_pk: SecretString,

    /// Private key to use to execute batches on L1
    /// Can be arbitrary funded address - execute submission is permissionless.
    // TODO: Pre-configured value, to be removed
    #[config(default_t = "0xbe5bef5a85f4b4f53d80b3750372a96be94d6780024c21a39cc3805832ca9611".into())]
    pub operator_execute_pk: SecretString,

    /// Max fee per gas we are willing to spend (in gwei).
    // 100 gwei was chosen as a reasonable threshold on Sepolia. In the observed period of 2024/07 to
    // 2025/07 it was exceeded twice:
    // * 214 gwei on 2025/03/10
    // * 2244 gwei from 2024/09/25 to 2024/10/07 (long spike with an average of ~200 gwei)
    //
    // Additionally, on Ethereum mainnet, gas price never exceeded 52 gwei over the same period of time.
    #[config(default_t = 101)]
    pub max_fee_per_gas_gwei: u64,

    /// Max priority fee per gas we are willing to spend (in gwei).
    // 2 gwei was chosen with Sepolia in mind. Median 50%-percentile priority fee in the observed
    // block range (8823177 to 8824177, mined on 2025/23/07) was 61mwei so 2 gwei should be enough
    // with >30x capacity.
    #[config(default_t = 2)]
    pub max_priority_fee_per_gas_gwei: u64,

    /// Max number of commands (to commit/prove/execute one batch) to be processed at a time.
    #[config(default_t = 16)]
    pub command_limit: usize,

    /// How often to poll L1 for new blocks.
    #[config(default_t = Duration::from_millis(100))]
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
