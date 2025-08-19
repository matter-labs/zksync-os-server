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
    #[config(alias = "operator_private_key", default_t = "0x85dddaefe4c7dbe9d9718e115ef7b3d73cef6dc52b25b2d9cc1df8a73edbfe2e".into())]
    pub operator_commit_pk: SecretString,

    /// Private key to use to submit proofs to L1
    /// Can be arbitrary funded address - proof submission is permissionless.
    // TODO: Pre-configured value, to be removed
    #[config(default_t = "0x23554649ed89ad65782dc6d56d81b0d11e631b0e8acc88b3a2d77cee4c2bf871".into())]
    pub operator_prove_pk: SecretString,

    /// Private key to use to execute batches on L1
    /// Can be arbitrary funded address - execute submission is permissionless.
    // TODO: Pre-configured value, to be removed
    #[config(default_t = "0x6afc8796d288cabd9f6c95ce0ee5cc76aa0dfc06f2f19a3a91746062ecd63d91".into())]
    pub operator_execute_pk: SecretString,

    /// L1 address of `Bridgehub` contract. This is an entrypoint into L1 discoverability so most
    /// other contracts should be discoverable through it.
    // TODO: Pre-configured value, to be removed
    #[config(with = Serde![str], default_t = "0xf8fd5b98e60fd511ce5af1f988e62eafbd4ddb09".parse().unwrap())]
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
