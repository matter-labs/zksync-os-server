// The code in this file was copied from reth with some minor changes. Source:
// https://github.com/paradigmxyz/reth/blob/fcf58cb5acc2825e7c046f6741e90a8c5dab7847/crates/rpc/rpc-api/src/web3.rs

use alloy::primitives::{B256, Bytes};
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;

/// Web3 rpc interface.
#[cfg_attr(not(feature = "server"), rpc(client, namespace = "web3"))]
#[cfg_attr(feature = "server", rpc(server, client, namespace = "web3"))]
pub trait Web3Api {
    /// Returns current client version.
    #[method(name = "clientVersion")]
    async fn client_version(&self) -> RpcResult<String>;

    /// Returns sha3 of the given data.
    #[method(name = "sha3")]
    fn sha3(&self, input: Bytes) -> RpcResult<B256>;
}
