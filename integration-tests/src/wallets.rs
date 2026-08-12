use crate::config::ChainLayout;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct WalletEntry {
    pub private_key: String,
}

#[derive(Debug, Deserialize)]
struct ChainWallets {
    pub operator_prove_sk: WalletEntry,
}

fn wallets_path(layout: ChainLayout<'_>) -> PathBuf {
    PathBuf::from(
        std::env::var("WORKSPACE_DIR").expect("WORKSPACE_DIR environment variable is not set"),
    )
    .join("local-chains")
    .join(layout.protocol_version())
    .join("default")
    .join("wallets.yaml")
}

/// Loads the private key holding REVERTER_ROLE on the ValidatorTimelock from the fixture's
/// `default/wallets.yaml`. zk-deployer grants that role to the chain's prove operator.
pub fn load_reverter_private_key(layout: ChainLayout<'_>, chain_id: u64) -> anyhow::Result<String> {
    let path = wallets_path(layout);
    let wallets: HashMap<String, serde_yaml::Value> =
        serde_yaml::from_str(&std::fs::read_to_string(&path)?)?;
    let chain: ChainWallets = serde_yaml::from_value(
        wallets
            .get(&chain_id.to_string())
            .ok_or_else(|| anyhow::anyhow!("no wallets for chain {chain_id} in {path:?}"))?
            .clone(),
    )?;
    Ok(chain.operator_prove_sk.private_key)
}
