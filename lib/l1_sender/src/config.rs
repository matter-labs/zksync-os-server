use alloy::consensus::constants::GWEI_TO_WEI;
use alloy::primitives::Address;
use smart_config::Serde;
use smart_config::value::SecretString;
use smart_config::{DescribeConfig, DeserializeConfig};

/// Configuration of L1 sender.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct L1SenderConfig {
    /// L1's JSON RPC API.
    #[config(default_t = "ws://localhost:8545".into())]
    pub l1_api_url: String,

    /// L2 chain ID to operate on.
    #[config(default_t = 270)]
    pub chain_id: u64,

    /// Private key of the L2 chain's operator address.
    // TODO: Pre-configured value, to be removed (pk for 0x5927c313861c01b82a026e35d93cc787e5356c0f)
    #[config(default_t = "0xc9ee945b2f6d4c462a743f5af3904a4ee78aec0218f1f4f3c53d0bfbf809b520".into())]
    pub operator_private_key: SecretString,

    /// L1 address of `Bridgehub` contract. This is an entrypoint into L1 discoverability so most
    /// other contracts should be discoverable through it.
    // TODO: Pre-configured value, to be removed
    #[config(with = Serde![str], default_t = "0x4b37536b9824c4a4cf3d15362135e346adb7cb9c".parse().unwrap())]
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
