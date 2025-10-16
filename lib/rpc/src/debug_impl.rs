use crate::eth_call_handler::{EthCallError, EthCallHandler};
use crate::result::{ToRpcResult, unimplemented_rpc_err};
use crate::{ReadRpcStorage, sandbox};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::genesis::ChainConfig;
use alloy::primitives::{B256, BlockHash, Bytes, TxHash};
use alloy::rpc::types::trace::geth::{
    GethDebugBuiltInTracerType, GethDebugTracerType, GethDebugTracingCallOptions,
    GethDebugTracingOptions, GethTrace, TraceResult,
};
use alloy::rpc::types::{Bundle, StateContext, TransactionRequest};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use std::ops::Range;
use zksync_os_rpc_api::debug::DebugApiServer;
use zksync_os_storage_api::{RepositoryError, StateError};

pub struct DebugNamespace<RpcStorage> {
    storage: RpcStorage,
    eth_call_handler: EthCallHandler<RpcStorage>,
}

impl<RpcStorage: ReadRpcStorage> DebugNamespace<RpcStorage> {
    pub fn new(storage: RpcStorage, eth_call_handler: EthCallHandler<RpcStorage>) -> Self {
        Self {
            storage,
            eth_call_handler,
        }
    }
}

impl<RpcStorage: ReadRpcStorage> DebugNamespace<RpcStorage> {
    fn debug_trace_block_by_id_impl(
        &self,
        block_id: BlockId,
        txs_range: Option<Range<usize>>,
        opts: Option<GethDebugTracingOptions>,
    ) -> DebugResult<Vec<TraceResult>> {
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
        let Some(block) = self.storage.get_block_by_id(block_id)? else {
            return Err(DebugError::BlockNotFound);
        };
        if block.number == 0 {
            // Short-circuit for genesis block.
            return Ok(Vec::new());
        }

        let Some(block_context) = self.storage.replay_storage().get_context(block.number) else {
            tracing::error!(
                block_number = block.number,
                "Could not load block's context"
            );
            return Err(DebugError::InternalError);
        };
        let mut txs = Vec::new();
        for tx_hash in
            &block.body.transactions[txs_range.unwrap_or(0..block.body.transactions.len())]
        {
            let Some(tx) = self.storage.repository().get_transaction(*tx_hash)? else {
                tracing::error!(
                    ?tx_hash,
                    block_number = block.number,
                    "Could not find transaction that was included in block"
                );
                return Err(DebugError::InternalError);
            };
            txs.push(tx);
        }
        let prev_state_view = self.storage.state_view_at(block.number - 1)?;
        match sandbox::call_trace(txs, block_context, prev_state_view, call_config) {
            Ok(calls) => Ok(calls
                .into_iter()
                .zip(&block.body.transactions)
                .map(|(call, tx_hash)| {
                    TraceResult::new_success(GethTrace::CallTracer(call), Some(*tx_hash))
                })
                .collect()),
            Err(err) => {
                tracing::error!(?err, "Failed to trace transaction");
                Err(DebugError::InternalError)
            }
        }
    }

    fn debug_trace_transaction_impl(
        &self,
        requested_tx_hash: TxHash,
        opts: Option<GethDebugTracingOptions>,
    ) -> DebugResult<GethTrace> {
        let Some(tx_meta) = self
            .storage
            .repository()
            .get_transaction_meta(requested_tx_hash)?
        else {
            return Err(DebugError::TransactionNotFound);
        };
        let block_number = tx_meta.block_number;

        self.debug_trace_block_by_id_impl(
            block_number.into(),
            Some(0..tx_meta.tx_index_in_block as usize + 1),
            opts,
        )
        // We only need last transaction's traces
        .map(|mut traces| traces.pop().unwrap())
        .and_then(|x| match x {
            TraceResult::Success { result, .. } => Ok(result),
            TraceResult::Error { error, .. } => {
                tracing::error!(?error, "Failed to trace transaction");
                Err(DebugError::InternalError)
            }
        })
    }

    fn debug_trace_call_impl(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
        opts: Option<GethDebugTracingCallOptions>,
    ) -> DebugResult<GethTrace> {
        let opts = opts.unwrap_or_default();
        let GethDebugTracingCallOptions {
            tracing_options,
            state_overrides,
            block_overrides,
        } = opts;
        let Some(tracer) = tracing_options.tracer else {
            return Err(DebugError::UnsupportedDefaultTracer);
        };
        if tracer != GethDebugTracerType::BuiltInTracer(GethDebugBuiltInTracerType::CallTracer) {
            return Err(DebugError::UnsupportedTracer(tracer));
        }
        let call_config = tracing_options
            .tracer_config
            .into_call_config()
            .map_err(|_| DebugError::InvalidTracerConfig)?;
        Ok(self.eth_call_handler.call_trace_impl(
            request,
            block_id,
            call_config,
            state_overrides,
            block_overrides.map(Box::new),
        )?)
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
        block: BlockHash,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        self.debug_trace_block_by_id_impl(block.into(), None, opts)
            .to_rpc_result()
    }

    async fn debug_trace_block_by_number(
        &self,
        block: BlockNumberOrTag,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        self.debug_trace_block_by_id_impl(block.into(), None, opts)
            .to_rpc_result()
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
        request: TransactionRequest,
        block_id: Option<BlockId>,
        opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<GethTrace> {
        self.debug_trace_call_impl(request, block_id, opts)
            .to_rpc_result()
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
    /// Thrown when a requested block is not found
    #[error("block not found")]
    BlockNotFound,
    /// Internal server error not exposed to user
    #[error("internal error")]
    InternalError,

    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Call(#[from] EthCallError),
}
