//! `zks` finality methods: report how far a block / transaction / batch has progressed through the
//! L1 finality pipeline (pending → committed → executed → finalized), plus a single-call snapshot of
//! all finality frontiers.

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::TxHash;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use serde::{Deserialize, Serialize};

/// Stage reached in the L1 finality pipeline.
///
/// Progression, from least to most final: `Pending` → `Committed` → `Executed` → `Finalized`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalityStage {
    /// Produced by the sequencer but not yet committed to L1; matches the `latest` block tag.
    Pending,
    /// Committed to L1; matches the `safe` block tag.
    Committed,
    /// Executed on L1 — the execute transaction is mined, but its L1 block may not be finalized yet.
    Executed,
    /// Executed, and the L1 execute transaction is itself finalized (irreversible); matches the
    /// `finalized` block tag.
    Finalized,
}

/// Finality status of a single block, transaction, or batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinalityResponse {
    /// Highest finality stage reached.
    pub stage: FinalityStage,
    /// L2 block this status refers to: the queried block, the transaction's block, or the batch's
    /// last block. `None` when it cannot be resolved.
    pub block_number: Option<u64>,
    /// L1 batch number, when known. Populated once the batch is executed — the batch↔block mapping
    /// is only persisted for executed batches.
    pub batch_number: Option<u64>,
}

/// Snapshot of every finality frontier the node tracks, in a single call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeFinalityStatus {
    /// Latest block sealed by the sequencer; matches `latest`.
    pub last_sealed_block: u64,
    /// Last L2 block committed to L1; matches `safe`.
    pub last_committed_block: u64,
    /// Last batch committed to L1.
    pub last_committed_batch: u64,
    /// Last L2 block whose batch was executed on L1.
    pub last_executed_block: u64,
    /// Last batch executed on L1.
    pub last_executed_batch: u64,
    /// Last executed L2 block whose L1 execute transaction is finalized; matches `finalized`.
    pub last_finalized_executed_block: u64,
    /// Last executed batch whose L1 execute transaction is finalized.
    pub last_finalized_executed_batch: u64,
}

#[cfg_attr(not(feature = "server"), rpc(client, namespace = "zks"))]
#[cfg_attr(feature = "server", rpc(server, client, namespace = "zks"))]
pub trait ZksFinalityApi {
    /// Returns the finality status of a block (by number or tag), or `null` if the block does not
    /// exist on this node.
    #[method(name = "getBlockFinality")]
    fn get_block_finality(&self, block: BlockNumberOrTag) -> RpcResult<Option<FinalityResponse>>;

    /// Returns the finality status of a transaction (by hash), or `null` if it is unknown to this
    /// node.
    #[method(name = "getTransactionFinality")]
    fn get_transaction_finality(&self, tx_hash: TxHash) -> RpcResult<Option<FinalityResponse>>;

    /// Returns the finality status of an L1 batch (by number). Batches above the committed frontier
    /// report [`FinalityStage::Pending`].
    #[method(name = "getBatchFinality")]
    fn get_batch_finality(&self, batch_number: u64) -> RpcResult<FinalityResponse>;

    /// Returns every finality frontier the node tracks (sealed / committed / executed / finalized)
    /// in a single call.
    #[method(name = "getFinalityStatus")]
    fn get_finality_status(&self) -> RpcResult<NodeFinalityStatus>;
}
