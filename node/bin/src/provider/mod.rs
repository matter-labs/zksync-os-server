mod latency;
mod metrics;
mod retry;

use self::latency::LatencyLayer;
use self::retry::RetryLayer;
use alloy::network::{Ethereum, EthereumWallet};
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{Provider, ProviderBuilder, WalletProvider};
use alloy::rpc::client::RpcClient;
use alloy::signers::local::PrivateKeySigner;
use std::time::Duration;

pub async fn build_node_provider(
    rpc_url: &str,
    poll_interval: Duration,
) -> FillProvider<
    impl TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet> + 'static,
    impl Provider<Ethereum> + Clone + 'static,
> {
    let client = RpcClient::builder()
        .layer(LatencyLayer)
        .layer(RetryLayer)
        .connect(rpc_url)
        .await
        .expect("failed to connect to L1 api")
        .with_poll_interval(poll_interval);
    ProviderBuilder::new()
        .wallet(EthereumWallet::new(PrivateKeySigner::random()))
        .connect_client(client)
}
