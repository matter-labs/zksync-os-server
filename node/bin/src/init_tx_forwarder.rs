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

/// The consensus observer's forwarder: transactions received over this node's RPC
/// travel round-robin to the validators' RPCs (an observer holds no leader turns,
/// so it can never include them itself).
pub async fn build_round_robin_tx_forwarder(urls: &[String]) -> TxForwarder {
    let mut endpoints = Vec::with_capacity(urls.len());
    for url in urls {
        let provider = ProviderBuilder::new()
            .connect(url)
            .await
            .unwrap_or_else(|err| panic!("could not connect to forward target {url}: {err}"))
            .erased();
        endpoints.push(TxForwardEndpoint::new(url.clone(), provider));
    }
    TxForwarder::round_robin(endpoints)
}
