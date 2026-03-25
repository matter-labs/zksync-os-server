use crate::config::ProviderConfig;
use alloy::network::{Ethereum, EthereumWallet};
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{Provider, ProviderBuilder, WalletProvider};
use alloy::rpc::client::{BuiltInConnectionString, RpcClient};
use alloy::signers::local::PrivateKeySigner;
use alloy::transports::http::Http;
use alloy::transports::layers::{
    FallbackLayer, RateLimitRetryPolicy, RetryBackoffLayer, RetryPolicy,
};
use alloy::transports::{BoxTransport, TransportConnect, TransportError, TransportErrorKind};
use futures::future::try_join_all;
use std::num::NonZeroUsize;
use std::time::Duration;
use tower::{Layer, ServiceBuilder};

/// It should practically never take this long for a request,
/// so if it does it's better to return an error than keep hanging
const RPC_TIMEOUT_BEFORE_RETRY: Duration = Duration::from_secs(60);

#[derive(Debug, Copy, Clone, Default)]
struct OptimisticRetryPolicy(RateLimitRetryPolicy);

impl RetryPolicy for OptimisticRetryPolicy {
    fn should_retry(&self, error: &TransportError) -> bool {
        if self.0.should_retry(error) {
            return true;
        }
        match error {
            TransportError::Transport(TransportErrorKind::HttpError(e)) => {
                // By default, only 429 and 503 are considered retryable; we also observe intermittent
                // 500 and 502 on Alchemy that are very likely retriable.
                e.status == 500 || e.status == 502
            }
            TransportError::Transport(TransportErrorKind::Custom(e)) => {
                let msg = e.to_string();
                // Internal `reqwest` error that can occur when node experiences intermittent
                // networking issues.
                msg.contains("error sending request")
            }
            TransportError::ErrorResp(e) => {
                // Internal error as observed on Infura
                e.code == -32603
            }
            _ => false,
        }
    }

    fn backoff_hint(&self, error: &TransportError) -> Option<Duration> {
        self.0.backoff_hint(error)
    }
}

/// Build BoxTransport from a url, adds a timeout
/// see `RPC_TIMEOUT_BEFORE_RETRY`
async fn transport_for_url(rpc_url: &str) -> Result<BoxTransport, TransportError> {
    let connection = rpc_url.parse::<BuiltInConnectionString>()?;

    match connection {
        BuiltInConnectionString::Http(url) => {
            let client = reqwest::Client::builder()
                .timeout(RPC_TIMEOUT_BEFORE_RETRY)
                .build()
                .map_err(TransportErrorKind::custom)?;
            Ok(BoxTransport::new(Http::with_client(client, url)))
        }
        _ => connection.get_transport().await,
    }
}

/// Creates a provider from list of RPC URLs
/// The provider can change the endpoint used based on stability and latency
pub async fn build_node_provider(
    rpc_urls: &[String],
    provider_config: ProviderConfig,
) -> Result<
    FillProvider<
        impl TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet> + 'static,
        impl Provider<Ethereum> + Clone + 'static,
    >,
    TransportError,
> {
    assert!(
        !rpc_urls.is_empty(),
        "at least one RPC URL must be configured to build a provider"
    );

    let retry_layer = RetryBackoffLayer::new_with_policy(
        provider_config.retry_attempt_limit,
        provider_config.retry_backoff_period.as_millis() as u64,
        u64::MAX, // compute units per second, considering it unlimited for now
        OptimisticRetryPolicy::default(),
    );

    let transports =
        try_join_all(rpc_urls.iter().map(|rpc_url| transport_for_url(rpc_url))).await?;

    let fallback_layer = FallbackLayer::default()
        .with_active_transport_count(NonZeroUsize::new(1).expect("1 is non-zero"));
    let transport = ServiceBuilder::new()
        .layer(retry_layer)
        .service(fallback_layer.layer(transports));
    let client = RpcClient::new(transport, false);

    Ok(ProviderBuilder::new()
        .wallet(EthereumWallet::new(PrivateKeySigner::random()))
        .connect_client(client))
}
