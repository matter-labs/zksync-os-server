use crate::config::{
    BaseTokenPriceUpdaterConfig, BatchVerificationConfig, BatcherConfig, Config,
    ExternalPriceApiClientConfig, FeeConfig, GasAdjusterConfig, GeneralConfig, GenesisConfig,
    InteropFeeUpdaterConfig, L1SenderConfig, L1WatcherConfig, MempoolConfig,
    MempoolTxValidatorConfig, NetworkConfig, ObservabilityConfig, ProverApiConfig,
    ProverInputGeneratorConfig, RpcConfig, SequencerConfig, StatusServerConfig,
};
use smart_config::{ConfigRepository, ConfigSources, Json, Yaml};
use std::fs;
use std::path::{Path, PathBuf};
use zksync_os_types::ConfigFormat;

/// Builds the runtime [`Config`] by parsing all supported sections from the repository,
/// applying node-role-specific adjustments, and validating distinct L1 operator addresses.
pub async fn build_external_config(repo: ConfigRepository<'_>) -> Config {
    let general_config = repo
        .single::<GeneralConfig>()
        .expect("Failed to load general config")
        .parse()
        .expect("Failed to parse general config");

    let network_config = repo
        .single::<NetworkConfig>()
        .expect("Failed to load network config")
        .parse()
        .expect("Failed to parse network config");

    let genesis_config = repo
        .single::<GenesisConfig>()
        .expect("Failed to load genesis config")
        .parse()
        .expect("Failed to parse genesis config");

    let rpc_config = repo
        .single::<RpcConfig>()
        .expect("Failed to load rpc config")
        .parse()
        .expect("Failed to parse rpc config");

    let mempool_config = repo
        .single::<MempoolConfig>()
        .expect("Failed to load mempool config")
        .parse()
        .expect("Failed to parse mempool config");

    let tx_validator_config = repo
        .single::<MempoolTxValidatorConfig>()
        .expect("Failed to load tx validator config")
        .parse()
        .expect("Failed to parse tx validator config");

    let sequencer_config = repo
        .single::<SequencerConfig>()
        .expect("Failed to load sequencer config")
        .parse()
        .expect("Failed to parse sequencer config");

    let mut l1_sender_config = repo
        .single::<L1SenderConfig>()
        .expect("Failed to load L1 sender config")
        .parse()
        .expect("Failed to parse L1 sender config");
    if general_config.node_role.is_external() {
        // This line just enforces that we expect no pubdata mode for external node.
        l1_sender_config.pubdata_mode = None;
    }

    let l1_watcher_config = repo
        .single::<L1WatcherConfig>()
        .expect("Failed to load L1 watcher config")
        .parse()
        .expect("Failed to parse L1 watcher config");

    let batcher_config = repo
        .single::<BatcherConfig>()
        .expect("Failed to load L1 watcher config")
        .parse()
        .expect("Failed to parse L1 watcher config");

    let prover_input_generator_config = repo
        .single::<ProverInputGeneratorConfig>()
        .expect("Failed to load ProverInputGenerator config")
        .parse()
        .expect("Failed to parse ProverInputGenerator config");

    let prover_api_config = repo
        .single::<ProverApiConfig>()
        .expect("Failed to load prover api config")
        .parse()
        .expect("Failed to parse prover api config");

    let status_server_config = repo
        .single::<StatusServerConfig>()
        .expect("Failed to load status server config")
        .parse()
        .expect("Failed to parse status server config");

    let observability_config = repo
        .single::<ObservabilityConfig>()
        .expect("Failed to load observability config")
        .parse()
        .expect("Failed to parse observability config");

    let gas_adjuster_config = repo
        .single::<GasAdjusterConfig>()
        .expect("Failed to load gas adjuster config")
        .parse()
        .expect("Failed to parse gas adjuster config");

    let batch_verification_config = repo
        .single::<BatchVerificationConfig>()
        .expect("Failed to load batch verification config")
        .parse()
        .expect("Failed to parse batch verification config");

    let base_token_price_updater_config = repo
        .single::<BaseTokenPriceUpdaterConfig>()
        .expect("Failed to load base token price updater config")
        .parse()
        .expect("Failed to parse base token price updater config");

    let interop_fee_updater_config = repo
        .single::<InteropFeeUpdaterConfig>()
        .expect("Failed to load interop fee updater config")
        .parse()
        .expect("Failed to parse interop fee updater config");

    // Parse this config only for Main Nodes. External Nodes never start the base token price updater.
    let external_price_api_client_config = if general_config.node_role.is_main() {
        Some(
            repo.single::<ExternalPriceApiClientConfig>()
                .expect("Failed to load external price API client config")
                .parse()
                .expect("Failed to parse external price API client config"),
        )
    } else {
        None
    };

    let fee_config = repo
        .single::<FeeConfig>()
        .expect("Failed to load fee config")
        .parse()
        .expect("Failed to parse fee config");

    // Validate that operator signers resolve to different Ethereum addresses (Main Node only).
    // Resolving the address for GCP KMS keys requires a network call, but is necessary to catch
    // duplicates across different backends (e.g. a local key and a KMS key for the same address).
    if let (Some(commit), Some(prove), Some(execute)) = (
        &l1_sender_config.operator_commit_sk,
        &l1_sender_config.operator_prove_sk,
        &l1_sender_config.operator_execute_sk,
    ) {
        let commit_addr = commit
            .address()
            .await
            .expect("failed to resolve commit operator address");
        let prove_addr = prove
            .address()
            .await
            .expect("failed to resolve prove operator address");
        let execute_addr = execute
            .address()
            .await
            .expect("failed to resolve execute operator address");
        if commit_addr == prove_addr || prove_addr == execute_addr || execute_addr == commit_addr {
            panic!(
                "Operator addresses for commit, prove and execute must be different, \
                 got commit={commit_addr}, prove={prove_addr}, execute={execute_addr}"
            );
        }
    }

    Config {
        general_config,
        network_config,
        genesis_config,
        rpc_config,
        mempool_config,
        tx_validator_config,
        sequencer_config,
        l1_sender_config,
        l1_watcher_config,
        batcher_config,
        prover_input_generator_config,
        prover_api_config,
        status_server_config,
        observability_config,
        gas_adjuster_config,
        batch_verification_config,
        base_token_price_updater_config,
        interop_fee_updater_config,
        external_price_api_client_config,
        fee_config,
    }
}

/// Loads JSON / YAML config files into [`ConfigSources`] in the provided order, inferring the
/// format from each path extension.
pub fn load_config_file_sources(config_sources: &mut ConfigSources, config_paths: &[PathBuf]) {
    for config_path in config_paths {
        let source_name = config_path.to_string_lossy();
        let config_contents = fs::read_to_string(config_path)
            .unwrap_or_else(|_| panic!("Failed to read config file from path '{source_name}'"));

        // Detect file format based on extension
        let path = Path::new(config_path);
        match ConfigFormat::from_path(path) {
            ConfigFormat::Yaml => {
                let config_yaml: serde_yaml::Mapping = serde_yaml::from_str(&config_contents)
                    .unwrap_or_else(|_| {
                        panic!("Failed to parse YAML config file from path '{source_name}'")
                    });
                config_sources.push(Yaml::new(source_name.as_ref(), config_yaml).unwrap_or_else(
                    |_| panic!("Failed to create YAML config source from path '{source_name}'"),
                ));
            }
            ConfigFormat::Json => {
                let config_json: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_str(&config_contents).unwrap_or_else(|_| {
                        panic!("Failed to parse JSON config file from path '{source_name}'")
                    });
                config_sources.push(Json::new(source_name.as_ref(), config_json));
            }
        }
    }
}
