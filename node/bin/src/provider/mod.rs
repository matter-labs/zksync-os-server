mod latency;
mod metrics;
mod retry;

use self::latency::LatencyLayer;
use self::retry::RetryLayer;
use crate::config::ProviderConfig;
use alloy::network::{Ethereum, EthereumWallet};
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{Provider, ProviderBuilder, WalletProvider};
use alloy::rpc::client::RpcClient;
use alloy::signers::local::PrivateKeySigner;
use vise::{EncodeLabelSet, EncodeLabelValue};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue, EncodeLabelSet)]
#[metrics(label = "provider", rename_all = "snake_case")]
pub(crate) enum ProviderKind {
    L1,
    Gateway,
}

pub(crate) async fn build_node_provider(
    config: &ProviderConfig,
    provider: ProviderKind,
) -> FillProvider<
    impl TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet> + 'static,
    impl Provider<Ethereum> + Clone + 'static,
> {
    let client = RpcClient::builder()
        .layer(LatencyLayer { provider })
        .layer(RetryLayer {
            provider,
            max_retries: config.max_retries,
            backoff: config.retry_backoff,
        })
        .connect(&config.rpc_url)
        .await
        .expect("failed to connect to L1 api")
        .with_poll_interval(config.rpc_poll_interval);
    ProviderBuilder::new()
        .wallet(EthereumWallet::new(PrivateKeySigner::random()))
        .connect_client(client)
}
