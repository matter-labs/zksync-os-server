use alloy::primitives::Address;
use smart_config::Serde;
use smart_config::value::SecretString;
use smart_config::{DescribeConfig, DeserializeConfig};
use std::path::PathBuf;

/// Configuration of L1 sender.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
#[config(derive(Default))]
pub struct L1SenderConfig {
    /// Path to the root directory for RocksDB.
    #[config(default_t = "./db/node1".into())]
    pub rocks_db_path: PathBuf,

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
}
