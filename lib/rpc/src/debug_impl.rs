use crate::ReadRpcStorage;
use crate::result::unimplemented_rpc_err;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::genesis::ChainConfig;
use alloy::primitives::{B256, BlockHash, Bytes, TxHash};
use alloy::rpc::types::trace::geth::{
    GethDebugTracingCallOptions, GethDebugTracingOptions, GethTrace, TraceResult,
};
use alloy::rpc::types::{Bundle, StateContext, TransactionRequest};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use zksync_os_rpc_api::debug::DebugApiServer;

pub struct DebugNamespace<RpcStorage> {
    storage: RpcStorage,
}

impl<RpcStorage> DebugNamespace<RpcStorage> {
    pub fn new(storage: RpcStorage) -> Self {
        Self { storage }
    }
}

impl<RpcStorage: ReadRpcStorage> DebugNamespace<RpcStorage> {}

#[async_trait]
impl<RpcStorage: ReadRpcStorage> DebugApiServer for DebugNamespace<RpcStorage> {
    async fn raw_header(&self, _block_id: BlockId) -> RpcResult<Bytes> {
        Err(unimplemented_rpc_err())
    }

    async fn raw_block(&self, _block_id: BlockId) -> RpcResult<Bytes> {
        Err(unimplemented_rpc_err())
    }

    async fn raw_transaction(&self, _hash: TxHash) -> RpcResult<Option<Bytes>> {
        Err(unimplemented_rpc_err())
    }

    async fn raw_transactions(&self, _block_id: BlockId) -> RpcResult<Vec<Bytes>> {
        Err(unimplemented_rpc_err())
    }

    async fn raw_receipts(&self, _block_id: BlockId) -> RpcResult<Vec<Bytes>> {
        Err(unimplemented_rpc_err())
    }

    async fn debug_trace_block(
        &self,
        _rlp_block: Bytes,
        _opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        Err(unimplemented_rpc_err())
    }

    async fn debug_trace_block_by_hash(
        &self,
        _block: BlockHash,
        _opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        Err(unimplemented_rpc_err())
    }

    async fn debug_trace_block_by_number(
        &self,
        _block: BlockNumberOrTag,
        _opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        Err(unimplemented_rpc_err())
    }

    async fn debug_trace_transaction(
        &self,
        _tx_hash: TxHash,
        _opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<GethTrace> {
        Err(unimplemented_rpc_err())
    }

    async fn debug_trace_call(
        &self,
        _request: TransactionRequest,
        _block_id: Option<BlockId>,
        _opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<GethTrace> {
        Err(unimplemented_rpc_err())
    }

    async fn debug_trace_call_many(
        &self,
        _bundles: Vec<Bundle>,
        _state_context: Option<StateContext>,
        _opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<Vec<Vec<GethTrace>>> {
        Err(unimplemented_rpc_err())
    }

    async fn debug_chain_config(&self) -> RpcResult<ChainConfig> {
        Err(unimplemented_rpc_err())
    }

    async fn debug_code_by_hash(
        &self,
        _hash: B256,
        _block_id: Option<BlockId>,
    ) -> RpcResult<Option<Bytes>> {
        Err(unimplemented_rpc_err())
    }
}
