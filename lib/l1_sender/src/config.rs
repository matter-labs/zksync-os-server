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
    #[config(default_t = "ws://localhost:8545".into())]
    pub l1_api_url: String,

    /// Private key to commit batches to L1
    /// Must be consistent with the operator key set on the contract (permissioned!)
    // TODO: Pre-configured value, to be removed (pk for 0x1561ac2f6ac6f3073c2180094ec67898b6fea40f)
    #[config(alias = "operator_private_key", default_t = "0x2338cc241022abdc9f3bcb0999d2336bdc1d7402560a5619d168b4bb88282c52".into())]
    pub operator_commit_pk: SecretString,

    /// Private key to use to submit proofs to L1
    /// Can be arbitrary funded address - proof submission is permissionless.
    // TODO: Pre-configured value, to be removed (pk for 0xa61464658AfeAf65CccaaFD3a512b69A83B77618)
    #[config(default_t = "0xac1e735be8536c6534bb4f17f06f6afc73b2b5ba84ac2cfb12f7461b20c0bbe3".into())]
    pub operator_prove_pk: SecretString,

    /// Private key to use to execute batches on L1
    /// Can be arbitrary funded address - proof submission is permissionless.
    // TODO: Pre-configured value, to be removed (pk for 0x0d43eb5b8a47ba8900d84aa36656c92024e9772e)
    #[config(default_t = "0xd293c684d884d56f8d6abd64fc76757d3664904e309a0645baf8522ab6366d9e".into())]
    pub operator_execute_pk: SecretString,

    /// L1 address of `Bridgehub` contract. This is an entrypoint into L1 discoverability so most
    /// other contracts should be discoverable through it.
    // TODO: Pre-configured value, to be removed
    #[config(with = Serde![str], default_t = "0x2bb295fe80bfcc2a9336402a5ad5ac099784b44f".parse().unwrap())]
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
