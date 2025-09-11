use std::{fs, path::Path, str::FromStr};

use anyhow::{Context, anyhow};
use serde_yaml::Value;
use zksync_os_l1_sender::config::L1SenderConfig;

use crate::config::{GeneralConfig, GenesisConfig};

pub struct ZkStackConfig {
    pub config_dir: String,
}

impl ZkStackConfig {
    pub fn new(config_dir: String) -> Self {
        Self { config_dir }
    }

    fn get_yaml_file(&self, file_name: &str) -> anyhow::Result<Value> {
        let cfg_path = std::path::Path::new(&self.config_dir).join(file_name);
        let text = fs::read_to_string(cfg_path).context(format!("Failed to read {file_name}"))?;
        let val: Value = serde_yaml::from_str(&text)?;
        Ok(val)
    }

    fn get_private_key(name: &str, entry: &Value) -> anyhow::Result<String> {
        entry
            .get(name)
            .and_then(|v| v.get("private_key").and_then(Value::as_str))
            .map(|s| s.to_string())
            .context(format!("Failed to parse {name} from entry"))
    }

    /// Update the configs based off the values from the yaml files.
    pub fn update(
        &self,
        general_config: &mut GeneralConfig,
        l1_sender_config: &mut L1SenderConfig,
        genesis_config: &mut GenesisConfig,
    ) -> anyhow::Result<()> {
        let zkstack_yaml = self.get_yaml_file("ZkStack.yaml")?;

        general_config.rocks_db_path = Path::new(&self.config_dir).join("db");

        let chain_id = zkstack_yaml
            .get("chain_id")
            .and_then(Value::as_u64)
            .context("Failed to parse chain_id")?;

        genesis_config.chain_id = chain_id;

        let wallets_yaml = self.get_yaml_file("configs/wallets.yaml")?;

        let operator = Self::get_private_key("operator", &wallets_yaml)?;
        let blob_operator = Self::get_private_key("blob_operator", &wallets_yaml)?;
        let deployer = Self::get_private_key("deployer", &wallets_yaml)?;

        l1_sender_config.operator_commit_pk = blob_operator.into();
        l1_sender_config.operator_prove_pk = operator.into();
        // TODO: this is not great, but we don't have a third wallet here. What should we use?
        l1_sender_config.operator_execute_pk = deployer.into();

        let contracts_yaml = self.get_yaml_file("configs/contracts.yaml")?;

        let ecosystem_contracts = contracts_yaml
            .get("ecosystem_contracts")
            .ok_or_else(|| anyhow!("Failed to get ecosystem from contracts.yaml"))?;

        let bridgehub_address = ecosystem_contracts
            .get("bridgehub_proxy_addr")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .context("Failed to parse bridgehub address")?;

        genesis_config.bridgehub_address =
            alloy::primitives::Address::from_str(&bridgehub_address)?;

        // ports

        Ok(())
    }
}
