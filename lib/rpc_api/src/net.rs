use alloy::primitives::U64;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;

/// net_* rpc interface
#[cfg_attr(not(feature = "server"), rpc(client, namespace = "net"))]
#[cfg_attr(feature = "server", rpc(server, client, namespace = "net"))]
pub trait NetApi {
    /// Returns the chain ID of the current network.
    #[method(name = "version")]
    async fn version(&self) -> RpcResult<Option<U64>>;
}
