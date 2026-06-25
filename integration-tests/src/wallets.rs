use crate::config::ChainLayout;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct WalletEntry {
    pub private_key: String,
}

#[derive(Debug, Deserialize)]
struct ChainWallets {
    pub operator: WalletEntry,
}

fn chain_wallets_path(layout: ChainLayout<'_>, chain_id: u64) -> PathBuf {
    PathBuf::from(
        std::env::var("WORKSPACE_DIR").expect("WORKSPACE_DIR environment variable is not set"),
    )
    .join("local-chains")
    .join(layout.protocol_version())
    .join("multi_chain")
    .join(format!("wallets_{chain_id}.yaml"))
}

/// Loads the `operator.private_key` from `wallets_<chain_id>.yaml` for the given chain layout.
/// The operator is the address that holds REVERTER_ROLE on the ValidatorTimelock.
pub fn load_operator_private_key(layout: ChainLayout<'_>, chain_id: u64) -> anyhow::Result<String> {
    let path = chain_wallets_path(layout, chain_id);
    let wallets: ChainWallets = serde_yaml::from_str(&std::fs::read_to_string(&path)?)?;
    Ok(wallets.operator.private_key)
}
