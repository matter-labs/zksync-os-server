use std::path::PathBuf;
use std::sync::LazyLock;

use smart_config::{ConfigRepository, ConfigSources, Json};
use zksync_os_server::config::{Config, GenesisConfig};
use zksync_os_server::default_protocol_version::{NEXT_PROTOCOL_VERSION, PROTOCOL_VERSION};

fn load_default_config(version: &str) -> Config {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").expect("WORKSPACE_DIR environment variable is not set");
    let config_path = format!("{workspace_dir}/local-chains/{version}/default/config.json");
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
        Some(format!("{workspace_dir}/local-chains/{version}/default/genesis.json").into());

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

static DEFAULT_CONFIG_V30: LazyLock<Config> =
    LazyLock::new(|| load_default_config(PROTOCOL_VERSION));

static DEFAULT_CONFIG_V31: LazyLock<Config> =
    LazyLock::new(|| load_default_config(NEXT_PROTOCOL_VERSION));

pub fn get_default_config_v30() -> &'static Config {
    &DEFAULT_CONFIG_V30
}

pub fn get_default_config_v31() -> &'static Config {
    &DEFAULT_CONFIG_V31
}

pub fn get_default_l1_state_path() -> String {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").expect("WORKSPACE_DIR environment variable is not set");
    format!("{workspace_dir}/local-chains/{PROTOCOL_VERSION}/default/zkos-l1-state.json")
}

pub fn get_multiple_chains_l1_state_path() -> String {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").expect("WORKSPACE_DIR environment variable is not set");
    PathBuf::from(workspace_dir)
        .join("local-chains")
        .join(NEXT_PROTOCOL_VERSION)
        .join("multi_chain")
        .join("zkos-l1-state.json")
        .to_string_lossy()
        .to_string()
}

/// Load chain configuration from a specific chain config file in the multiple-chains directory.
/// Returns the Config with the chain ID and other settings from the file.
pub fn get_chain_config(chain_index: usize) -> Config {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").expect("WORKSPACE_DIR environment variable is not set");
    // Map chain index to chain ID (0 -> 6565, 1 -> 6566, etc.)
    let chain_id = 6565 + chain_index as u64;
    let config_path = PathBuf::from(&workspace_dir)
        .join("local-chains")
        .join(NEXT_PROTOCOL_VERSION)
        .join("multi_chain")
        .join(format!("chain_{chain_id}.json"))
        .to_string_lossy()
        .to_string();

    let config_schema = Config::schema();
    let mut config_sources = ConfigSources::default();
    let config_contents = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|_| panic!("Failed to read config file: {config_path}"));

    let config_json: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&config_contents).expect("Failed to parse config file");
    config_sources.push(Json::new(&config_path, config_json));

    let config_repo = ConfigRepository::new(&config_schema).with_all(config_sources);
    let mut genesis_config: GenesisConfig = config_repo.single().unwrap().parse().unwrap();
    genesis_config.genesis_input_path = Some(
        format!("{workspace_dir}/local-chains/{NEXT_PROTOCOL_VERSION}/default/genesis.json").into(),
    );

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
