// The code in this file was copied from reth with some minor changes. Source:
// https://github.com/paradigmxyz/reth/blob/fcf58cb5acc2825e7c046f6741e90a8c5dab7847/crates/rpc/rpc-eth-api/src/pubsub.rs

use alloy::rpc::types::pubsub::{Params, SubscriptionKind};
use jsonrpsee::proc_macros::rpc;

/// Ethereum pub-sub rpc interface.
#[rpc(server, namespace = "eth")]
pub trait EthPubSubApi {
    /// Create an ethereum subscription for the given params
    #[subscription(
        name = "subscribe" => "subscription",
        unsubscribe = "unsubscribe",
        item = alloy::rpc::types::pubsub::SubscriptionResult<
            alloy::rpc::types::Transaction<zksync_os_types::ZkEnvelope>
        >
    )]
    async fn subscribe(
        &self,
        kind: SubscriptionKind,
        params: Option<Params>,
    ) -> jsonrpsee::core::SubscriptionResult;
}
