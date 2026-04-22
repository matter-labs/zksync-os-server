use alloy::rpc::types::txpool::{TxpoolContent, TxpoolInspect, TxpoolStatus};
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;

use crate::types::ZkApiTransaction;

/// `txpool` RPC interface.
///
/// See <https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-txpool> for reference.
#[cfg_attr(not(feature = "server"), rpc(client, namespace = "txpool"))]
#[cfg_attr(feature = "server", rpc(server, client, namespace = "txpool"))]
pub trait TxpoolApi {
    /// Returns a textual summary of all transactions currently pending for inclusion in the next
    /// block(s), as well as the ones that are being scheduled for future execution only.
    ///
    /// See <https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-txpool#txpool_inspect>.
    #[method(name = "inspect")]
    async fn inspect(&self) -> RpcResult<TxpoolInspect>;

    /// Returns the exact details of all transactions currently pending for inclusion in the next
    /// block(s), as well as the ones that are being scheduled for future execution only.
    ///
    /// See <https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-txpool#txpool_content>.
    #[method(name = "content")]
    async fn content(&self) -> RpcResult<TxpoolContent<ZkApiTransaction>>;

    /// Returns the number of transactions currently pending for inclusion in the next block(s), as
    /// well as the ones that are being scheduled for future execution only.
    ///
    /// See <https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-txpool#txpool_status>.
    #[method(name = "status")]
    async fn status(&self) -> RpcResult<TxpoolStatus>;
}
