use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, U64};
use backon::{ExponentialBuilder, Retryable};
use jsonrpsee::core::ClientError;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use std::future::Future;
use std::time::Duration;
use zksync_os_genesis::GenesisInput;
use zksync_os_rpc_api::eth::EthApiClient;
use zksync_os_rpc_api::types::ZkApiBlock;
use zksync_os_rpc_api::zks::ZksApiClient;

/// No cap: wait for the main node instead of crash-looping.
const RETRY: ExponentialBuilder = ExponentialBuilder::new()
    .with_min_delay(Duration::from_secs(1))
    .with_max_delay(Duration::from_secs(30))
    .without_max_times();

/// Whether `err` means "couldn't reach the main node" (worth retrying) rather than "the main node
/// answered with an error" (e.g. an old main node missing a method) or a local decoding error.
fn is_transient(err: &ClientError) -> bool {
    matches!(
        err,
        ClientError::Transport(_)
            | ClientError::RequestTimeout
            | ClientError::RestartNeeded(_)
            | ClientError::ServiceDisconnect
    )
}

async fn with_retry<T, Fut>(call: impl FnMut() -> Fut) -> Result<T, ClientError>
where
    Fut: Future<Output = Result<T, ClientError>>,
{
    call.retry(RETRY)
        .when(is_transient)
        .notify(|err, after| {
            tracing::warn!(%err, ?after, "main node unreachable; retrying in {after:?}: {err}")
        })
        .await
}

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
        with_retry(|| self.rpc.get_bridgehub_contract()).await
    }

    pub async fn bytecode_supplier_contract(&self) -> Result<Address, ClientError> {
        with_retry(|| self.rpc.get_bytecode_supplier_contract()).await
    }

    pub async fn chain_id(&self) -> Result<Option<U64>, ClientError> {
        with_retry(|| self.rpc.chain_id()).await
    }

    pub async fn genesis_input(&self) -> Result<GenesisInput, ClientError> {
        with_retry(|| self.rpc.get_genesis()).await
    }

    pub async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> Result<Option<ZkApiBlock>, ClientError> {
        with_retry(|| self.rpc.block_by_number(number, full)).await
    }
}
