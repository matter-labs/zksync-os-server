use crate::config::ProviderConfig;
use alloy::network::{Ethereum, EthereumWallet};
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{Provider, ProviderBuilder, WalletProvider};
use alloy::rpc::client::RpcClient;
use alloy::signers::local::PrivateKeySigner;
use alloy::transports::layers::{RateLimitRetryPolicy, RetryBackoffLayer, RetryPolicy};
use alloy::transports::{TransportError, TransportErrorKind};
use std::time::Duration;

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

pub async fn build_node_provider(
    rpc_url: &str,
    provider_config: &ProviderConfig,
) -> FillProvider<
    impl TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet> + 'static,
    impl Provider<Ethereum> + Clone + 'static,
> {
    let retry_layer = RetryBackoffLayer::new_with_policy(
        provider_config.max_retries,
        provider_config.retry_backoff.as_millis() as u64,
        u64::MAX, // compute units per second, considering it unlimited for now
        OptimisticRetryPolicy::default(),
    );
    let client = RpcClient::builder()
        .layer(retry_layer)
        .connect(rpc_url)
        .await
        .expect("failed to connect to L1 api");
    ProviderBuilder::new()
        .wallet(EthereumWallet::new(PrivateKeySigner::random()))
        .connect_client(client)
}
