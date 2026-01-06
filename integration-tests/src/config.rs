const CONFIG_PATH: &str = concat!(env!("WORKSPACE_DIR"), "/local-chains/v30/config.json");
const CHAIN_CONFIG: &str = include_str!(concat!(
    env!("WORKSPACE_DIR"),
    "/local-chains/v30/config.json"
));

use smart_config::{ConfigRepository, ConfigSources, Json};
use zksync_os_server::config::Config;

pub fn get_default_config() -> Config {
    let config_schema = Config::schema();
    let mut config_sources = ConfigSources::default();
    let config_contents = std::fs::read_to_string(CONFIG_PATH)
        .expect("Failed to read config file from provided path");

    let config_json: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&config_contents)
            .expect("Failed to parse config file from provided path");
    config_sources.push(Json::new(CONFIG_PATH, config_json));

    let config_repo = ConfigRepository::new(&config_schema).with_all(config_sources);

    Config {
        genesis_config: config_repo.single().unwrap().parse().unwrap(),
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

pub fn get_chain_id() -> u64 {
    let chain_config_json: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(CHAIN_CONFIG)
            .expect("Failed to parse chain config file from provided path");

    chain_config_json
        .get("genesis")
        .and_then(|g| g.pointer("/chain_id"))
        .and_then(|v| v.as_u64())
        .expect("chain_id is missing in the genesis config")
}

pub fn get_bridge_hub_supplier_address() -> String {
    let chain_config_json: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(CHAIN_CONFIG)
            .expect("Failed to parse chain config file from provided path");

    chain_config_json
        .get("genesis") // Get the top level object
        .and_then(|g| g.pointer("/bytecode_supplier_address"))
        .and_then(|v| v.as_str())
        .expect("bytecode_supplier_address is missing in the genesis config")
        .to_string()
}
