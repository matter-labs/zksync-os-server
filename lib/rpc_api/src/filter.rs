// The code in this file was copied from reth with some minor changes. Source:
// https://github.com/paradigmxyz/reth/blob/fcf58cb5acc2825e7c046f6741e90a8c5dab7847/crates/rpc/rpc-eth-api/src/filter.rs

use alloy::rpc::types::{Filter, FilterChanges, FilterId, Log, PendingTransactionFilterKind};
use alloy_json_rpc::RpcObject;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;

/// Rpc Interface for poll-based ethereum filter API.
#[cfg_attr(not(feature = "server"), rpc(client, namespace = "eth"))]
#[cfg_attr(feature = "server", rpc(server, client, namespace = "eth"))]
pub trait EthFilterApi<T: RpcObject> {
    /// Creates a new filter and returns its id.
    #[method(name = "newFilter")]
    async fn new_filter(&self, filter: Filter) -> RpcResult<FilterId>;

    /// Creates a new block filter and returns its id.
    #[method(name = "newBlockFilter")]
    async fn new_block_filter(&self) -> RpcResult<FilterId>;

    /// Creates a pending transaction filter and returns its id.
    #[method(name = "newPendingTransactionFilter")]
    async fn new_pending_transaction_filter(
        &self,
        kind: Option<PendingTransactionFilterKind>,
    ) -> RpcResult<FilterId>;

    /// Returns all filter changes since last poll.
    #[method(name = "getFilterChanges")]
    async fn filter_changes(&self, id: FilterId) -> RpcResult<FilterChanges<T>>;

    /// Returns all logs matching given filter (in a range 'from' - 'to').
    #[method(name = "getFilterLogs")]
    async fn filter_logs(&self, id: FilterId) -> RpcResult<Vec<Log>>;

    /// Uninstalls filter.
    #[method(name = "uninstallFilter")]
    async fn uninstall_filter(&self, id: FilterId) -> RpcResult<bool>;

    /// Returns logs matching given filter object.
    #[method(name = "getLogs")]
    async fn logs(&self, filter: Filter) -> RpcResult<Vec<Log>>;
}
