// The code in this file was copied from reth with some minor changes. Source:
// https://github.com/paradigmxyz/reth/blob/fcf58cb5acc2825e7c046f6741e90a8c5dab7847/crates/rpc/rpc-api/src/otterscan.rs

use crate::types::ZkApiTransaction;
use alloy::eips::eip1898::LenientBlockNumberOrTag;
use alloy::primitives::{Address, BlockHash, Bytes, TxHash};
use alloy::rpc::types::Header;
use alloy::rpc::types::trace::otterscan::{
    BlockDetails, ContractCreator, InternalOperation, OtsBlockTransactions, TraceEntry,
    TransactionsWithReceipts,
};
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;

#[cfg_attr(not(feature = "server"), rpc(client, namespace = "ots"))]
#[cfg_attr(feature = "server", rpc(server, client, namespace = "ots"))]
pub trait OtsApi {
    /// Get the block header by block number, required by otterscan.
    /// Otterscan currently requires this endpoint, used as:
    ///
    /// 1. check if the node is Erigon or not
    /// 2. get block header instead of the full block
    ///
    /// Ref: <https://github.com/otterscan/otterscan/blob/071d8c55202badf01804f6f8d53ef9311d4a9e47/src/useProvider.ts#L71>
    #[method(name = "getHeaderByNumber", aliases = ["erigon_getHeaderByNumber"])]
    async fn get_header_by_number(
        &self,
        block_number: LenientBlockNumberOrTag,
    ) -> RpcResult<Option<Header>>;

    /// Check if a certain address contains a deployed code.
    #[method(name = "hasCode")]
    async fn has_code(
        &self,
        address: Address,
        block_id: Option<LenientBlockNumberOrTag>,
    ) -> RpcResult<bool>;

    /// Very simple API versioning scheme. Every time we add a new capability, the number is
    /// incremented. This allows for Otterscan to check if the node contains all API it
    /// needs.
    #[method(name = "getApiLevel")]
    async fn get_api_level(&self) -> RpcResult<u64>;

    /// Return the internal ETH transfers inside a transaction.
    #[method(name = "getInternalOperations")]
    async fn get_internal_operations(&self, tx_hash: TxHash) -> RpcResult<Vec<InternalOperation>>;

    /// Given a transaction hash, returns its raw revert reason.
    #[method(name = "getTransactionError")]
    async fn get_transaction_error(&self, tx_hash: TxHash) -> RpcResult<Option<Bytes>>;

    /// Extract all variations of calls, contract creation and self-destructs and returns a call
    /// tree.
    #[method(name = "traceTransaction")]
    async fn trace_transaction(&self, tx_hash: TxHash) -> RpcResult<Option<Vec<TraceEntry>>>;

    /// Tailor-made and expanded version of `eth_getBlockByNumber` for block details page in
    /// Otterscan.
    #[method(name = "getBlockDetails")]
    async fn get_block_details(
        &self,
        block_number: LenientBlockNumberOrTag,
    ) -> RpcResult<BlockDetails>;

    /// Tailor-made and expanded version of `eth_getBlockByHash` for block details page in
    /// Otterscan.
    #[method(name = "getBlockDetailsByHash")]
    async fn get_block_details_by_hash(&self, block_hash: BlockHash) -> RpcResult<BlockDetails>;

    /// Get paginated transactions for a certain block. Also remove some verbose fields like logs.
    #[method(name = "getBlockTransactions")]
    async fn get_block_transactions(
        &self,
        block_number: LenientBlockNumberOrTag,
        page_number: usize,
        page_size: usize,
    ) -> RpcResult<OtsBlockTransactions<ZkApiTransaction>>;

    /// Gets paginated inbound/outbound transaction calls for a certain address.
    #[method(name = "searchTransactionsBefore")]
    async fn search_transactions_before(
        &self,
        address: Address,
        block_number: LenientBlockNumberOrTag,
        page_size: usize,
    ) -> RpcResult<TransactionsWithReceipts<ZkApiTransaction>>;

    /// Gets paginated inbound/outbound transaction calls for a certain address.
    #[method(name = "searchTransactionsAfter")]
    async fn search_transactions_after(
        &self,
        address: Address,
        block_number: LenientBlockNumberOrTag,
        page_size: usize,
    ) -> RpcResult<TransactionsWithReceipts<ZkApiTransaction>>;

    /// Gets the transaction hash for a certain sender address, given its nonce.
    #[method(name = "getTransactionBySenderAndNonce")]
    async fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> RpcResult<Option<TxHash>>;

    /// Gets the transaction hash and the address who created a contract.
    #[method(name = "getContractCreator")]
    async fn get_contract_creator(&self, address: Address) -> RpcResult<Option<ContractCreator>>;
}
