use crate::result::{ToRpcResult, unimplemented_rpc_err};
use crate::{ReadRpcStorage, sandbox};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::genesis::ChainConfig;
use alloy::primitives::{B256, BlockHash, BlockNumber, Bytes, TxHash};
use alloy::rpc::types::trace::geth::{
    GethDebugBuiltInTracerType, GethDebugTracerType, GethDebugTracingCallOptions,
    GethDebugTracingOptions, GethTrace, TraceResult,
};
use alloy::rpc::types::{Bundle, StateContext, TransactionRequest};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use zksync_os_rpc_api::debug::DebugApiServer;
use zksync_os_storage_api::RepositoryError;

pub struct DebugNamespace<RpcStorage> {
    storage: RpcStorage,
}

impl<RpcStorage> DebugNamespace<RpcStorage> {
    pub fn new(storage: RpcStorage) -> Self {
        Self { storage }
    }
}

impl<RpcStorage: ReadRpcStorage> DebugNamespace<RpcStorage> {
    fn debug_trace_transaction_impl(
        &self,
        tx_hash: TxHash,
        opts: Option<GethDebugTracingOptions>,
    ) -> DebugResult<GethTrace> {
        let opts = opts.unwrap_or_default();
        let Some(tracer) = opts.tracer else {
            return Err(DebugError::UnsupportedDefaultTracer);
        };
        if tracer != GethDebugTracerType::BuiltInTracer(GethDebugBuiltInTracerType::CallTracer) {
            return Err(DebugError::UnsupportedTracer(tracer));
        }
        let call_config = opts
            .tracer_config
            .into_call_config()
            .map_err(|_| DebugError::InvalidTracerConfig)?;
        let Some(tx) = self.storage.repository().get_transaction(tx_hash)? else {
            return Err(DebugError::TransactionNotFound);
        };
        let Some(tx_meta) = self.storage.repository().get_transaction_meta(tx_hash)? else {
            return Err(DebugError::TransactionNotFound);
        };
        let block_number = tx_meta.block_number - 1;
        let Some(block_context) = self.storage.replay_storage().get_context(block_number) else {
            return Err(DebugError::BlockNotAvailable(block_number));
        };
        let state_view = self
            .storage
            .state()
            .state_view_at_block(block_number)
            .map_err(|_| DebugError::BlockNotAvailable(block_number))?;
        // todo: execute previous transactions from current block before simulating requested transaction
        match sandbox::call_trace(tx, block_context, state_view, call_config) {
            Ok(call) => Ok(GethTrace::CallTracer(call)),
            Err(err) => {
                tracing::error!(?tx_hash, ?err, "failed to trace transaction");
                Err(DebugError::InternalError)
            }
        }
    }
}

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
        tx_hash: TxHash,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<GethTrace> {
        self.debug_trace_transaction_impl(tx_hash, opts)
            .to_rpc_result()
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

/// `debug` namespace result type.
pub type DebugResult<Ok> = Result<Ok, DebugError>;

/// General `debug` namespace errors.
#[derive(Debug, thiserror::Error)]
pub enum DebugError {
    // todo: support default tracer
    /// Unsupported default tracer
    #[error("default struct log tracer is not supported")]
    UnsupportedDefaultTracer,
    /// Unsupported tracer type
    #[error("tracer {} is not supported", .0.as_str())]
    UnsupportedTracer(GethDebugTracerType),
    /// When the tracer config does not match the tracer
    #[error("invalid tracer config")]
    InvalidTracerConfig,
    /// Thrown when a requested transaction is not found
    #[error("transaction not found")]
    TransactionNotFound,
    /// Block is no longer available (e.g., it was compacted)
    #[error("block {0} is not available")]
    BlockNotAvailable(BlockNumber),
    /// Internal server error not exposed to user
    #[error("internal error")]
    InternalError,

    #[error(transparent)]
    Repository(#[from] RepositoryError),
}
