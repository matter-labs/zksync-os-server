use alloy::network::EthereumWallet;
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

pub async fn build_node_l1_provider(
    l1_rpc_url: &str,
) -> impl Provider + WalletProvider<Wallet = EthereumWallet> + Clone + 'static {
    let retry_layer = RetryBackoffLayer::new_with_policy(
        2,        // max retries, excluding the initial attempt
        200,      // backoff in ms,
        u64::MAX, // compute units per second, considering it unlimited for now
        OptimisticRetryPolicy::default(),
    );
    let client = RpcClient::builder()
        .layer(retry_layer)
        .connect(l1_rpc_url)
        .await
        .expect("failed to connect to L1 api");
    ProviderBuilder::new()
        .wallet(EthereumWallet::new(PrivateKeySigner::random()))
        .connect_client(client)
}
