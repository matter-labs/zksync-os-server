use alloy::providers::{Provider, ProviderBuilder};
use zksync_os_rpc::{TxForwardEndpoint, TxForwarder};

pub async fn build_static_tx_forwarder(url: &str) -> TxForwarder {
    let provider = ProviderBuilder::new()
        .connect(url)
        .await
        .expect("could not connect to main node RPC")
        .erased();
    TxForwarder::static_target(TxForwardEndpoint::new(url.to_owned(), provider))
}
