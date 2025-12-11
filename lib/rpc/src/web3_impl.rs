use alloy::primitives::{B256, Bytes, keccak256};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use zksync_os_metadata::NODE_ZKSYNC_OS_VERSION;
use zksync_os_rpc_api::web3::Web3ApiServer;

#[derive(Default)]
pub struct Web3Namespace;

#[async_trait]
impl Web3ApiServer for Web3Namespace {
    async fn client_version(&self) -> RpcResult<String> {
        Ok(NODE_ZKSYNC_OS_VERSION.to_string())
    }

    fn sha3(&self, input: Bytes) -> RpcResult<B256> {
        Ok(keccak256(input))
    }
}
