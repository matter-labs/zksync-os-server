use flate2::read::GzDecoder;
use smart_config::{ConfigRepository, ConfigSources, Json, Yaml};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use zksync_os_server::config::{Config, GenesisConfig};
use zksync_os_types::ConfigFormat;

/// Layout of local chain directories.
#[derive(Debug, Clone, Copy)]
pub enum ChainLayout<'a> {
    /// local-chains/<version>/default/...
    Default { protocol_version: &'a str },
    /// local-chains/<version>/multi_chain/...
    MultiChain {
        protocol_version: &'a str,
        chain_index: usize, // 0 -> 6565, 1 -> 6566, ...
    },
}

impl<'a> ChainLayout<'a> {
    fn chain_id(self) -> Option<u64> {
        match self {
            ChainLayout::Default { .. } => None,
            ChainLayout::MultiChain { chain_index, .. } => {
                if chain_index == 0 {
                    Some(506u64)
                } else {
                    Some(6565u64 - 1 + chain_index as u64)
                }
            }
        }
    }

    fn protocol_version(self) -> &'a str {
        match self {
            ChainLayout::Default { protocol_version } => protocol_version,
            ChainLayout::MultiChain {
                protocol_version, ..
            } => protocol_version,
        }
    }

    fn dir(self) -> &'static str {
        match self {
            ChainLayout::Default { .. } => "default",
            ChainLayout::MultiChain { .. } => "multi_chain",
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
            ChainLayout::MultiChain { .. } => {
                let chain_id = self.chain_id().expect("multi-chain always has chain_id");
                self.base_dir().join(format!("chain_{chain_id}.yaml"))
            }
        }
    }

    pub(crate) fn l1_state(self) -> Vec<u8> {
        /// Cache decompressed L1 state to avoid re-decompressing ~70MB per test.
        static L1_STATE_CACHE: LazyLock<Mutex<HashMap<PathBuf, Vec<u8>>>> =
            LazyLock::new(|| Mutex::new(HashMap::new()));

        let compressed_path = self.protocol_dir().join("l1-state.json.gz");

        let cache = L1_STATE_CACHE.lock().unwrap();
        if let Some(cached) = cache.get(&compressed_path) {
            return cached.clone();
        }
        drop(cache);

        let data = std::fs::read(&compressed_path).expect("failed to read compressed L1 state");
        let mut decoder = GzDecoder::new(data.as_slice());
        let mut decoded_data = Vec::new();
        decoder
            .read_to_end(decoded_data.as_mut())
            .expect("failed to decompress L1 state");

        let mut cache = L1_STATE_CACHE.lock().unwrap();
        cache.insert(compressed_path, decoded_data.clone());
        decoded_data
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
        external_price_api_client_config: config_repo.single().unwrap().parse().unwrap(),
        fee_config: Default::default(),
    }
}
