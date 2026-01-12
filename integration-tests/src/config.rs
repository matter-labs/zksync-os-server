use std::sync::LazyLock;

use smart_config::{ConfigRepository, ConfigSources, Json};
use zksync_os_server::{
    config::{Config, GenesisConfig},
    config_constants::PROTOCOL_VERSION,
};

static DEFAULT_CONFIG: LazyLock<Config> = LazyLock::new(|| {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").expect("WORKSPACE_DIR environment variable is not set");
    let config_path = format!("{workspace_dir}/local-chains/{PROTOCOL_VERSION}/config.json");
    let config_schema = Config::schema();
    let mut config_sources = ConfigSources::default();
    let config_contents =
        std::fs::read_to_string(&config_path).expect("Failed to read config file");

    let config_json: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&config_contents).expect("Failed to parse config file");
    config_sources.push(Json::new(&config_path, config_json));

    let config_repo = ConfigRepository::new(&config_schema).with_all(config_sources);
    let mut genesis_config: GenesisConfig = config_repo.single().unwrap().parse().unwrap();
    genesis_config.genesis_input_path =
        Some(format!("{workspace_dir}/local-chains/{PROTOCOL_VERSION}/genesis.json").into());

    Config {
        genesis_config,
        l1_sender_config: config_repo.single().unwrap().parse().unwrap(),
        general_config: Default::default(),
        rpc_config: Default::default(),
        mempool_config: Default::default(),
        tx_validator_config: Default::default(),
        sequencer_config: Default::default(),
        l1_watcher_config: Default::default(),
        batcher_config: Default::default(),
        prover_input_generator_config: Default::default(),
        prover_api_config: Default::default(),
        status_server_config: Default::default(),
        observability_config: Default::default(),
        gas_adjuster_config: Default::default(),
        batch_verification_config: Default::default(),
    }
});

pub fn get_default_config() -> &'static Config {
    &DEFAULT_CONFIG
}

pub fn get_default_l1_state_path() -> String {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").expect("WORKSPACE_DIR environment variable is not set");
    format!("{workspace_dir}/local-chains/{PROTOCOL_VERSION}/zkos-l1-state.json")
}

pub fn get_multiple_chains_l1_state_path() -> String {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").expect("WORKSPACE_DIR environment variable is not set");
    format!("{workspace_dir}/local-chains/{PROTOCOL_VERSION}/multiple-chains/zkos-l1-state.json")
}

/// Load chain configuration from a specific chain config file in the multiple-chains directory.
/// Returns the Config with the chain ID and other settings from the file.
pub fn get_chain_config(chain_index: usize) -> Config {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").expect("WORKSPACE_DIR environment variable is not set");
    let chain_num = chain_index + 1;
    let config_path = format!(
        "{workspace_dir}/local-chains/{PROTOCOL_VERSION}/multiple-chains/chain{chain_num}.json"
    );

    let config_schema = Config::schema();
    let mut config_sources = ConfigSources::default();
    let config_contents = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|_| panic!("Failed to read config file: {config_path}"));

    let config_json: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&config_contents).expect("Failed to parse config file");
    config_sources.push(Json::new(&config_path, config_json));

    let config_repo = ConfigRepository::new(&config_schema).with_all(config_sources);
    let mut genesis_config: GenesisConfig = config_repo.single().unwrap().parse().unwrap();
    genesis_config.genesis_input_path =
        Some(format!("{workspace_dir}/local-chains/{PROTOCOL_VERSION}/genesis.json").into());

    Config {
        genesis_config,
        l1_sender_config: config_repo.single().unwrap().parse().unwrap(),
        general_config: Default::default(),
        rpc_config: Default::default(),
        mempool_config: Default::default(),
        tx_validator_config: Default::default(),
        sequencer_config: Default::default(),
        l1_watcher_config: Default::default(),
        batcher_config: Default::default(),
        prover_input_generator_config: Default::default(),
        prover_api_config: Default::default(),
        status_server_config: Default::default(),
        observability_config: Default::default(),
        gas_adjuster_config: Default::default(),
        batch_verification_config: Default::default(),
    }
}
