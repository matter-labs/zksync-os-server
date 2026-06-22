//! The JSON-RPC client an external node uses to talk to its main node.
//!
//! Every main-node RPC interaction an external node makes at startup goes through this one client:
//! resolving genesis config, locating the last common block, and fetching genesis input.

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, U64};
use jsonrpsee::core::ClientError;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use zksync_os_genesis::GenesisInput;
use zksync_os_rpc_api::eth::EthApiClient;
use zksync_os_rpc_api::types::ZkApiBlock;
use zksync_os_rpc_api::zks::ZksApiClient;

/// Client an external node uses to reach its main node over JSON-RPC.
#[derive(Clone, Debug)]
pub struct MainNodeClient {
    rpc: HttpClient,
}

impl MainNodeClient {
    pub fn new(url: &str) -> anyhow::Result<Self> {
        Ok(Self {
            rpc: HttpClientBuilder::new().build(url)?,
        })
    }

    pub async fn bridgehub_contract(&self) -> Result<Address, ClientError> {
        self.rpc.get_bridgehub_contract().await
    }

    pub async fn bytecode_supplier_contract(&self) -> Result<Address, ClientError> {
        self.rpc.get_bytecode_supplier_contract().await
    }

    pub async fn chain_id(&self) -> Result<Option<U64>, ClientError> {
        self.rpc.chain_id().await
    }

    pub async fn genesis_input(&self) -> Result<GenesisInput, ClientError> {
        self.rpc.get_genesis().await
    }

    pub async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> Result<Option<ZkApiBlock>, ClientError> {
        self.rpc.block_by_number(number, full).await
    }
}
