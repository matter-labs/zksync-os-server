// The code in this file was copied from reth with some minor changes. Source:
// https://github.com/paradigmxyz/reth/blob/fcf58cb5acc2825e7c046f6741e90a8c5dab7847/crates/rpc/rpc-api/src/debug.rs

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::genesis::ChainConfig;
use alloy::primitives::{B256, BlockHash, Bytes, TxHash};
use alloy::rpc::types::trace::geth::{
    GethDebugTracingCallOptions, GethDebugTracingOptions, GethTrace, TraceResult,
};
use alloy::rpc::types::{Bundle, StateContext, TransactionRequest};
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;

#[cfg_attr(not(feature = "server"), rpc(client, namespace = "debug"))]
#[cfg_attr(feature = "server", rpc(server, client, namespace = "debug"))]
pub trait DebugApi {
    /// Returns an RLP-encoded header.
    #[method(name = "getRawHeader")]
    async fn raw_header(&self, block_id: BlockId) -> RpcResult<Bytes>;

    /// Returns an RLP-encoded block.
    #[method(name = "getRawBlock")]
    async fn raw_block(&self, block_id: BlockId) -> RpcResult<Bytes>;

    /// Returns a EIP-2718 binary-encoded transaction.
    ///
    /// If this is a pooled EIP-4844 transaction, the blob sidecar is included.
    #[method(name = "getRawTransaction")]
    async fn raw_transaction(&self, hash: TxHash) -> RpcResult<Option<Bytes>>;

    /// Returns an array of EIP-2718 binary-encoded transactions for the given [`BlockId`].
    #[method(name = "getRawTransactions")]
    async fn raw_transactions(&self, block_id: BlockId) -> RpcResult<Vec<Bytes>>;

    /// Returns an array of EIP-2718 binary-encoded receipts.
    #[method(name = "getRawReceipts")]
    async fn raw_receipts(&self, block_id: BlockId) -> RpcResult<Vec<Bytes>>;

    /// The `debug_traceBlock` method will return a full stack trace of all invoked opcodes of all
    /// transaction that were included in this block.
    ///
    /// This expects an rlp encoded block
    ///
    /// Note, the parent of this block must be present, or it will fail. For the second parameter
    /// see [`GethDebugTracingOptions`] reference.
    #[method(name = "traceBlock")]
    async fn debug_trace_block(
        &self,
        rlp_block: Bytes,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>>;

    /// Similar to `debug_traceBlock`, `debug_traceBlockByHash` accepts a block hash and will replay
    /// the block that is already present in the database. For the second parameter see
    /// [`GethDebugTracingOptions`].
    #[method(name = "traceBlockByHash")]
    async fn debug_trace_block_by_hash(
        &self,
        block: BlockHash,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>>;

    /// Similar to `debug_traceBlockByHash`, `debug_traceBlockByNumber` accepts a block number
    /// [`BlockNumberOrTag`] and will replay the block that is already present in the database.
    /// For the second parameter see [`GethDebugTracingOptions`].
    #[method(name = "traceBlockByNumber")]
    async fn debug_trace_block_by_number(
        &self,
        block: BlockNumberOrTag,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>>;

    /// The `debug_traceTransaction` debugging method will attempt to run the transaction in the
    /// exact same manner as it was executed on the network. It will replay any transaction that
    /// may have been executed prior to this one before it will finally attempt to execute the
    /// transaction that corresponds to the given hash.
    #[method(name = "traceTransaction")]
    async fn debug_trace_transaction(
        &self,
        tx_hash: TxHash,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<GethTrace>;

    /// The `debug_traceCall` method lets you run an `eth_call` within the context of the given
    /// block execution using the final state of parent block as the base.
    ///
    /// The first argument (just as in `eth_call`) is a transaction request.
    /// The block can optionally be specified either by hash or by number as
    /// the second argument.
    /// The trace can be configured similar to `debug_traceTransaction`,
    /// see [`GethDebugTracingOptions`]. The method returns the same output as
    /// `debug_traceTransaction`.
    #[method(name = "traceCall")]
    async fn debug_trace_call(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
        opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<GethTrace>;

    /// The `debug_traceCallMany` method lets you run an `eth_callMany` within the context of the
    /// given block execution using the final state of parent block as the base followed by n
    /// transactions.
    ///
    /// The first argument is a list of bundles. Each bundle can overwrite the block headers. This
    /// will affect all transaction in that bundle.
    /// `BlockNumber` and `transaction_index` are optional. `Transaction_index`
    /// specifies the number of tx in the block to replay and -1 means all transactions should be
    /// replayed.
    /// The trace can be configured similar to `debug_traceTransaction`.
    /// State override apply to all bundles.
    ///
    /// This methods is similar to many `eth_callMany`, hence this returns nested lists of traces.
    /// Where the length of the outer list is the number of bundles and the length of the inner list
    /// (`Vec<GethTrace>`) is the number of transactions in the bundle.
    #[method(name = "traceCallMany")]
    async fn debug_trace_call_many(
        &self,
        bundles: Vec<Bundle>,
        state_context: Option<StateContext>,
        opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<Vec<Vec<GethTrace>>>;

    /// Returns the current chain config.
    #[method(name = "chainConfig")]
    async fn debug_chain_config(&self) -> RpcResult<ChainConfig>;

    /// Returns the code associated with a given hash at the specified block ID.
    /// If no block ID is provided, it defaults to the latest block.
    #[method(name = "codeByHash")]
    async fn debug_code_by_hash(
        &self,
        hash: B256,
        block_id: Option<BlockId>,
    ) -> RpcResult<Option<Bytes>>;
}
