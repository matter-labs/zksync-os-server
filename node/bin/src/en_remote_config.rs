use crate::config::GenesisConfig;
use alloy::primitives::Address;
use anyhow::Context;
use jsonrpsee::http_client::HttpClient;
use std::sync::Arc;
use zksync_os_genesis::{FileGenesisInputSource, GenesisInputSource};
use zksync_os_rpc_api::eth::EthApiClient;
use zksync_os_rpc_api::zks::ZksApiClient;

pub async fn load_remote_config(
    main_node_rpc_url: &str,
    en_local_genesis_config: &GenesisConfig,
) -> anyhow::Result<(Address, u64, Arc<dyn GenesisInputSource>)> {
    let main_node_rpc_client =
        jsonrpsee::http_client::HttpClientBuilder::new().build(main_node_rpc_url)?;

    let remote_bridgehub_address = main_node_rpc_client.get_bridgehub_contract().await?;
    if let Some(local_bridgehub_address) = en_local_genesis_config.bridgehub_address {
        anyhow::ensure!(
            remote_bridgehub_address == local_bridgehub_address,
            "Bridgehub address mismatch: remote = {remote_bridgehub_address}, local = {local_bridgehub_address}",
        );
    }

    let remote_chain_id: u64 = u64::from_be_bytes(
        main_node_rpc_client
            .chain_id()
            .await?
            .context("missing chain_id")?
            .to_be_bytes(),
    );
    if let Some(local_chain_id) = en_local_genesis_config.chain_id {
        anyhow::ensure!(
            remote_chain_id == local_chain_id,
            "chain id mismatch: remote = {remote_chain_id}, local = {local_chain_id}",
        );
    }

    let genesis_input_source = Arc::new(MainNodeGenesisInputSource::new(main_node_rpc_client));
    if let Some(local_genesis_path) = en_local_genesis_config.genesis_input_path.clone() {
        let remote_genesis_input = genesis_input_source.genesis_input().await?;
        let local_genesis_input = FileGenesisInputSource::new(local_genesis_path)
            .genesis_input()
            .await?;

        let remote_json = serde_json::to_string(&remote_genesis_input)?;
        let local_json = serde_json::to_string(&local_genesis_input)?;

        anyhow::ensure!(
            local_genesis_input == remote_genesis_input,
            "Genesis input mismatch: remote = {remote_json}, local = {local_json}",
        );
    }

    Ok((
        remote_bridgehub_address,
        remote_chain_id,
        genesis_input_source,
    ))
}

#[derive(Debug)]
pub struct MainNodeGenesisInputSource {
    rpc_client: HttpClient,
}

impl MainNodeGenesisInputSource {
    pub fn new(rpc_client: HttpClient) -> Self {
        Self { rpc_client }
    }
}

#[async_trait::async_trait]
impl GenesisInputSource for MainNodeGenesisInputSource {
    async fn genesis_input(&self) -> anyhow::Result<zksync_os_genesis::GenesisInput> {
        let genesis = self.rpc_client.get_genesis().await?;
        Ok(genesis)
    }
}
