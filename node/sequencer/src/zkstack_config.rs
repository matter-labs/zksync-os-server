use std::{fs, path::Path, str::FromStr};

use crate::config::{GenesisConfig, ProverApiConfig};
use anyhow::{Context, anyhow};
use serde_yaml::Value;
use zksync_os_l1_sender::config::L1SenderConfig;
use zksync_os_rpc::RpcConfig;
use zksync_os_sequencer::config::SequencerConfig;

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
        sequencer_config: &mut SequencerConfig,
        rpc_config: &mut RpcConfig,
        l1_sender_config: &mut L1SenderConfig,
        genesis_config: &mut GenesisConfig,
        prover_api_config: &mut ProverApiConfig,
    ) -> anyhow::Result<()> {
        let zkstack_yaml = self.get_yaml_file("ZkStack.yaml")?;

        sequencer_config.rocks_db_path = Path::new(&self.config_dir).join("db");

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

        l1_sender_config.bridgehub_address =
            alloy::primitives::Address::from_str(&bridgehub_address)?;

        // ports

        let general_yaml = self.get_yaml_file("configs/general.yaml")?;
        let api = general_yaml
            .get("api")
            .ok_or_else(|| anyhow!("Failed to get api from general.yaml"))?;

        let prometheus_port = api
            .get("prometheus")
            .and_then(|v| v.get("listener_port").and_then(Value::as_u64))
            .ok_or(anyhow!("Failed to get prometheus port"))?;

        sequencer_config.prometheus_port = prometheus_port as u16;

        let rpc_port = api
            .get("web3_json_rpc")
            .and_then(|v| v.get("http_port").and_then(Value::as_u64))
            .ok_or(anyhow!("Failed to get web3_json_rpc port"))?;

        rpc_config.address = format!("0.0.0.0:{rpc_port}");

        let merkle_port = api
            .get("merkle_tree")
            .and_then(|v| v.get("port").and_then(Value::as_u64))
            .ok_or(anyhow!("Failed to get merkle_tree port"))?;

        // FIXME: for now, use the merkle port for block replay.
        sequencer_config.block_replay_server_address = format!("0.0.0.0:{merkle_port}");

        let data_handler_port = general_yaml
            .get("data_handler")
            .and_then(|v| v.get("http_port").and_then(Value::as_u64))
            .ok_or(anyhow!("Failed to get data_handler port"))?;

        prover_api_config.address = format!("0.0.0.0:{data_handler_port}");

        Ok(())
    }
}
