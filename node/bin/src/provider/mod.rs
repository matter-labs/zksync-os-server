mod latency;
mod metrics;
mod retry;

use crate::config::ProviderConfig;
use alloy::network::EthereumWallet;
use alloy::providers::ProviderBuilder;
use alloy::rpc::client::RpcClient;
use alloy::signers::local::PrivateKeySigner;
use std::time::Duration;
use tower::ServiceBuilder;
use vise::{EncodeLabelSet, EncodeLabelValue};
use zksync_os_provider::NodeProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue, EncodeLabelSet)]
#[metrics(label = "provider", rename_all = "snake_case")]
pub(crate) enum ProviderKind {
    L1,
    Gateway,
    L1CustomRetries,
    GatewayCustomRetries,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProviderRetryConfig {
    pub(crate) max_retries: Option<u32>,
    pub(crate) retry_all_errors: bool,
    pub(crate) backoff: Duration,
}

impl ProviderRetryConfig {
    pub(crate) fn from_provider_config(config: &ProviderConfig) -> Self {
        Self {
            max_retries: Some(config.max_retries),
            retry_all_errors: false,
            backoff: config.retry_backoff,
        }
    }
}

pub(crate) async fn build_node_provider(
    config: &ProviderConfig,
    latest_poll_interval: Duration,
    finalized_poll_interval: Duration,
    log_cache_capacity: usize,
    provider: ProviderKind,
    retry_config: Option<ProviderRetryConfig>,
) -> NodeProvider {
    let retry_config =
        retry_config.unwrap_or_else(|| ProviderRetryConfig::from_provider_config(config));
    let provider_layers = ServiceBuilder::new()
        .layer_fn(move |inner| latency::LatencyService { inner, provider })
        .layer_fn(move |inner| retry::RetryService {
            inner,
            provider,
            max_retries: retry_config.max_retries,
            retry_all_errors: retry_config.retry_all_errors,
            backoff: retry_config.backoff,
        });

    let client = RpcClient::builder()
        .layer(provider_layers)
        .connect(&config.rpc_url)
        .await
        .expect("failed to connect to L1 api")
        .with_poll_interval(config.rpc_poll_interval);
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::new(PrivateKeySigner::random()))
        .connect_client(client);
    NodeProvider::new_with_features(
        provider,
        latest_poll_interval,
        finalized_poll_interval,
        log_cache_capacity,
    )
    .await
    .expect("failed to initialize node provider features")
}
