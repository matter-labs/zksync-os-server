//! Workload contract management: typed call encoding via inline `sol!`
//! interfaces, deployment bytecode read from the forge artifacts at runtime,
//! and a deploy-once registry cached in the work directory.
//!
//! Compiling this crate never needs forge — `build.rs` builds the artifacts
//! when forge is available, and a missing artifact is a runtime error naming
//! the fix, raised only when a contract workload is actually enabled.

use super::bank::Bank;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use anyhow::Context as _;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// Typed encoders for the workload calls. These mirror the contracts under
// `tools/chaos/contracts/src/`; the ABI lives here so the rig gets compile-time
// checked calldata without needing the artifacts at compile time.
alloy::sol! {
    interface IChaosERC20 {
        function mint(address to, uint256 value) external;
        function transfer(address to, uint256 value) external returns (bool);
        function approve(address spender, uint256 value) external returns (bool);
    }
    interface ICallMaze {
        function walk(uint256 seed, uint8 depth) external payable returns (bytes32);
    }
    interface IPrecompileGym {
        function exercise(uint8 id) external;
    }
    interface IContextProbe {
        function probe(address someEoa) external payable;
    }
    interface IGasGuzzler {
        function burn(uint8 mode, uint256 seed) external;
    }
    interface IReverter {
        function fail(uint8 mode, uint256 seed) external;
    }
}

/// Where `build.rs` leaves the forge artifacts.
const ARTIFACTS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/out");

/// Deploys workload contracts through the bank's deployer account, reusing
/// live deployments recorded in `<workdir>/load-contracts.json` when their
/// bytecode still matches.
pub struct Deployments {
    cache_path: PathBuf,
    cache: BTreeMap<String, CachedDeployment>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedDeployment {
    address: Address,
    bytecode_hash: B256,
}

impl Deployments {
    pub fn open(workdir: &Path) -> Deployments {
        let cache_path = workdir.join("load-contracts.json");
        let cache = std::fs::read_to_string(&cache_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Deployments { cache_path, cache }
    }

    /// Address of `name` (e.g. "CallMaze"), deploying it if this cluster does
    /// not already run the current bytecode.
    pub async fn ensure(&mut self, bank: &mut Bank, name: &str) -> anyhow::Result<Address> {
        let bytecode = deployment_bytecode(name)?;
        let bytecode_hash = keccak256(&bytecode);

        if let Some(cached) = self.cache.get(name)
            && cached.bytecode_hash == bytecode_hash
        {
            let live = bank
                .deployer
                .provider
                .get_code_at(cached.address)
                .await
                .map(|code| !code.is_empty())
                .unwrap_or(false);
            if live {
                return Ok(cached.address);
            }
        }

        println!("deploying {name}");
        let request = TransactionRequest::default()
            .with_chain_id(bank.chain_id)
            .with_from(bank.deployer.address)
            .with_deploy_code(bytecode)
            .with_nonce(bank.deployer.nonce)
            .with_gas_limit(5_000_000)
            .with_max_fee_per_gas(bank.fees.0)
            .with_max_priority_fee_per_gas(bank.fees.1);
        let pending = bank
            .deployer
            .send_patiently(request)
            .await
            .with_context(|| format!("deploying {name}"))?;
        bank.deployer.nonce += 1;
        let receipt = pending
            .get_receipt()
            .await
            .with_context(|| format!("awaiting the {name} deployment"))?;
        let address = receipt
            .contract_address
            .with_context(|| format!("{name} deployment receipt has no address"))?;
        anyhow::ensure!(receipt.status(), "{name} deployment reverted");

        self.cache.insert(
            name.to_string(),
            CachedDeployment {
                address,
                bytecode_hash,
            },
        );
        let text = serde_json::to_string_pretty(&self.cache)?;
        std::fs::write(&self.cache_path, text)?;
        Ok(address)
    }
}

/// Reads a contract's deployment bytecode from its forge artifact.
fn deployment_bytecode(name: &str) -> anyhow::Result<Vec<u8>> {
    let path = format!("{ARTIFACTS_DIR}/{name}.sol/{name}.json");
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "no artifact for {name} at {path}; install foundry (https://getfoundry.sh/) \
             and rebuild, or run `forge build` in tools/chaos/contracts",
        )
    })?;
    let artifact: serde_json::Value = serde_json::from_str(&text)?;
    let hex = artifact["bytecode"]["object"]
        .as_str()
        .with_context(|| format!("artifact for {name} has no bytecode.object"))?;
    let bytes = alloy::hex::decode(hex).with_context(|| format!("decoding {name} bytecode"))?;
    anyhow::ensure!(!bytes.is_empty(), "artifact for {name} has empty bytecode");
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guarded on the artifacts' presence so a forge-less checkout still passes
    /// the suite — build.rs already warned it skipped the contracts.
    #[test]
    fn artifacts_hold_deployable_bytecode() {
        if !Path::new(ARTIFACTS_DIR).exists() {
            eprintln!("skipping: no forge artifacts at {ARTIFACTS_DIR}");
            return;
        }
        for name in [
            "ChaosERC20",
            "CallMaze",
            "PrecompileGym",
            "ContextProbe",
            "GasGuzzler",
            "Reverter",
        ] {
            let code = deployment_bytecode(name).unwrap_or_else(|err| panic!("{name}: {err}"));
            assert!(!code.is_empty(), "{name}: empty bytecode");
        }
    }
}
