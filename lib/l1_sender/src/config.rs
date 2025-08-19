use alloy::consensus::constants::GWEI_TO_WEI;
use alloy::primitives::Address;
use smart_config::Serde;
use smart_config::value::SecretString;
use smart_config::{DescribeConfig, DeserializeConfig};

/// Configuration of L1 sender.
/// todo: consider renaming to L1Config and using in L1Watcher as well.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct L1SenderConfig {
    /// L1's JSON RPC API.
    #[config(default_t = "http://localhost:8545".into())]
    pub l1_api_url: String,

    /// Private key to commit batches to L1
    /// Must be consistent with the operator key set on the contract (permissioned!)
    // TODO: Pre-configured value, to be removed
    #[config(alias = "operator_private_key", default_t = "0xef73ef185e6576081f18e2471a151311b4b8192930aa16cbf35bb8032a7c7d25".into())]
    pub operator_commit_pk: SecretString,

    /// Private key to use to submit proofs to L1
    /// Can be arbitrary funded address - proof submission is permissionless.
    // TODO: Pre-configured value, to be removed
    #[config(default_t = "0x6a80582659b98209d028e6b419e594605918700e9a695b2d62597bc8de5a4000".into())]
    pub operator_prove_pk: SecretString,

    /// Private key to use to execute batches on L1
    /// Can be arbitrary funded address - execute submission is permissionless.
    // TODO: Pre-configured value, to be removed
    #[config(default_t = "0xed1b5cc0eb19856d4ad0486eef06c9d1bb21e9b08a140fdfa4534414ee45d04c".into())]
    pub operator_execute_pk: SecretString,

    /// L1 address of `Bridgehub` contract. This is an entrypoint into L1 discoverability so most
    /// other contracts should be discoverable through it.
    // TODO: Pre-configured value, to be removed
    #[config(with = Serde![str], default_t = "0xb02237aee96515ae7b70308ba4d95e4bae047079".parse().unwrap())]
    pub bridgehub_address: Address,

    /// Max fee per gas we are willing to spend (in gwei).
    // 100 gwei was chosen as a reasonable threshold on Sepolia. In the observed period of 2024/07 to
    // 2025/07 it was exceeded twice:
    // * 214 gwei on 2025/03/10
    // * 2244 gwei from 2024/09/25 to 2024/10/07 (long spike with an average of ~200 gwei)
    //
    // Additionally, on Ethereum mainnet, gas price never exceeded 52 gwei over the same period of time.
    #[config(default_t = 100)]
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
