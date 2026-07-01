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
}

pub(crate) async fn build_node_provider(
    config: &ProviderConfig,
    latest_poll_interval: Duration,
    finalized_poll_interval: Duration,
    log_cache_capacity: usize,
    provider: ProviderKind,
) -> NodeProvider {
    let max_retries = config.max_retries;
    let retry_backoff = config.retry_backoff;
    let provider_layers = ServiceBuilder::new()
        .layer_fn(move |inner| latency::LatencyService { inner, provider })
        .layer_fn(move |inner| retry::RetryService {
            inner,
            provider,
            max_retries,
            backoff: retry_backoff,
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
