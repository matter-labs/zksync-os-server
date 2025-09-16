use alloy::network::EthereumWallet;
use alloy::providers::{Provider, ProviderBuilder, WalletProvider};
use alloy::rpc::client::RpcClient;
use alloy::signers::local::PrivateKeySigner;
use alloy::transports::layers::RetryBackoffLayer;

pub async fn build_node_l1_provider(
    l1_rpc_url: &str,
) -> impl Provider + WalletProvider<Wallet = EthereumWallet> + Clone + 'static {
    let retry_layer = RetryBackoffLayer::new(
        2,        // max retries, excluding the initial attempt
        200,      // backoff in ms,
        u64::MAX, // compute units per second, considering it unlimited for now
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
