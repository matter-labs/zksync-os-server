use alloy::primitives::{B256, Bytes, keccak256};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use zksync_os_rpc_api::web3::Web3ApiServer;

const CLIENT_VERSION: &str = concat!(
    "zksync-os/v",
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR"),
    ".",
    env!("CARGO_PKG_VERSION_PATCH")
);

#[derive(Default)]
pub struct Web3Namespace;

#[async_trait]
impl Web3ApiServer for Web3Namespace {
    async fn client_version(&self) -> RpcResult<String> {
        Ok(CLIENT_VERSION.to_string())
    }

    fn sha3(&self, input: Bytes) -> RpcResult<B256> {
        Ok(keccak256(input))
    }
}
