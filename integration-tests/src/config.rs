use smart_config::{ConfigRepository, ConfigSources, Json, Yaml};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use zksync_os_server::config::{Config, GenesisConfig};
use zksync_os_types::ConfigFormat;

/// Layout of local chain directories.
#[derive(Debug, Clone, Copy)]
pub enum ChainLayout<'a> {
    /// local-chains/<version>/default/...
    Default { protocol_version: &'a str },
    /// local-chains/<version>/multi_chain/chain_506.yaml
    Gateway { protocol_version: &'a str },
    /// local-chains/<version>/multi_chain/chain_<id>.yaml for chains settling to the gateway.
    GatewayChain {
        protocol_version: &'a str,
        chain_index: usize, // 0 -> 6565, 1 -> 6566, ...
    },
}

impl<'a> ChainLayout<'a> {
    fn chain_id(self) -> Option<u64> {
        match self {
            ChainLayout::Default { .. } => None,
            ChainLayout::Gateway { .. } => Some(506),
            ChainLayout::GatewayChain { chain_index, .. } => Some(6565u64 + chain_index as u64),
        }
    }

    pub fn protocol_version(self) -> &'a str {
        match self {
            ChainLayout::Default { protocol_version } => protocol_version,
            ChainLayout::Gateway { protocol_version } => protocol_version,
            ChainLayout::GatewayChain {
                protocol_version, ..
            } => protocol_version,
        }
    }

    fn dir(self) -> &'static str {
        match self {
            ChainLayout::Default { .. } => "default",
            ChainLayout::Gateway { .. } | ChainLayout::GatewayChain { .. } => "multi_chain",
        }
    }

    fn protocol_dir(self) -> PathBuf {
        workspace_dir()
            .join("local-chains")
            .join(self.protocol_version())
    }

    fn base_dir(self) -> PathBuf {
        self.protocol_dir().join(self.dir())
    }

    fn config_path(self) -> PathBuf {
        match self {
            ChainLayout::Default { .. } => self.base_dir().join("config.yaml"),
            ChainLayout::Gateway { .. } | ChainLayout::GatewayChain { .. } => {
                let chain_id = self.chain_id().expect("multi-chain always has chain_id");
                self.base_dir().join(format!("chain_{chain_id}.yaml"))
            }
        }
    }

    /// Read the pre-decompressed L1 state JSON.
    /// Produced by `build.rs` locally, or by a CI step on remote runners.
    pub(crate) fn l1_state(self) -> Vec<u8> {
        let json_path = self.protocol_dir().join("l1-state.json");
        std::fs::read(&json_path).unwrap_or_else(|e| {
            panic!(
                "failed to read decompressed L1 state at {}: {e}\n\
                 hint: build.rs should produce this from l1-state.json.gz; \
                 on CI runners run `gunzip -k` first",
                json_path.display()
            )
        })
    }

    /// Genesis input is always taken from `<version>/default/genesis.json`
    fn genesis_input_path(self) -> PathBuf {
        workspace_dir()
            .join("local-chains")
            .join(self.protocol_version())
            .join("default")
            .join("genesis.json")
    }
}

/// Load a `Config` from either default or multi-chain layout.
pub fn load_chain_config(layout: ChainLayout<'_>) -> Config {
    let mut config = load_config_from_path(&layout.config_path());
    config.genesis_config.genesis_input_path = Some(layout.genesis_input_path());
    if let Some(ephemeral_state) = &config.general_config.ephemeral_state
        && ephemeral_state.is_relative()
    {
        config.general_config.ephemeral_state = Some(workspace_dir().join(ephemeral_state));
    }
    config
}

/// Workspace directory path, taken from WORKSPACE_DIR environment variable.
static WORKSPACE_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::var("WORKSPACE_DIR")
        .expect("WORKSPACE_DIR environment variable is not set")
        .into()
});

/// Get the workspace directory path.
fn workspace_dir() -> &'static Path {
    WORKSPACE_DIR.as_path()
}

/// Load config from the given path.
fn load_config_from_path(config_path: &Path) -> Config {
    let config_schema = Config::schema();
    let mut config_sources = ConfigSources::default();
    let config_contents = std::fs::read_to_string(config_path)
        .unwrap_or_else(|e| panic!("Failed to read config file {}: {e}", config_path.display()));
    let source_name = config_path.to_string_lossy();

    match ConfigFormat::from_path(config_path) {
        ConfigFormat::Yaml => {
            let config_yaml: serde_yaml::Mapping = serde_yaml::from_str(&config_contents)
                .expect("Failed to parse YAML config file from provided path");

            config_sources.push(
                Yaml::new(source_name.as_ref(), config_yaml)
                    .expect("Failed to create YAML config source"),
            );
        }
        ConfigFormat::Json => {
            let config_json: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&config_contents)
                    .expect("Failed to parse JSON config file from provided path");
            config_sources.push(Json::new(source_name.as_ref(), config_json));
        }
    }

    let config_repo = ConfigRepository::new(&config_schema).with_all(config_sources);
    let single = config_repo.single().unwrap();
    let genesis_config: GenesisConfig = single.parse().unwrap();

    Config {
        genesis_config,
        l1_sender_config: config_repo.single().unwrap().parse().unwrap(),
        general_config: config_repo.single().unwrap().parse().unwrap(),
        network_config: Default::default(),
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
        base_token_price_updater_config: config_repo.single().unwrap().parse().unwrap(),
        interop_fee_updater_config: Default::default(),
        external_price_api_client_config: Some(config_repo.single().unwrap().parse().unwrap()),
        fee_config: Default::default(),
        pipeline_health_config: Default::default(),
    }
}
