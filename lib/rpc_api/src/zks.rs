use crate::types::{BatchStorageProof, BlockMetadata, L2ToL1LogProof, LogProofTarget};
use alloy::primitives::{Address, B256, TxHash};
use alloy::rpc::types::Index;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use zksync_os_genesis::GenesisInput;

#[cfg_attr(not(feature = "server"), rpc(client, namespace = "zks"))]
#[cfg_attr(feature = "server", rpc(server, client, namespace = "zks"))]
pub trait ZksApi {
    #[method(name = "getBridgehubContract")]
    fn get_bridgehub_contract(&self) -> RpcResult<Address>;

    #[method(name = "getBytecodeSupplierContract")]
    fn get_bytecode_supplier_contract(&self) -> RpcResult<Address>;

    /// Returns the merkle proof for an L2->L1 log emitted in a given transaction.
    ///
    /// `proof_target` selects which root the proof anchors to (see [`LogProofTarget`]).
    /// If omitted, [`LogProofTarget::L1BatchRoot`] is used.
    #[method(name = "getL2ToL1LogProof")]
    async fn get_l2_to_l1_log_proof(
        &self,
        tx_hash: TxHash,
        index: Index,
        proof_target: Option<LogProofTarget>,
    ) -> RpcResult<Option<L2ToL1LogProof>>;

    #[method(name = "getGenesis")]
    async fn get_genesis(&self) -> RpcResult<GenesisInput>;

    #[method(name = "getBlockMetadataByNumber")]
    fn get_block_metadata_by_number(&self, block_number: u64) -> RpcResult<Option<BlockMetadata>>;

    #[method(name = "getProof", blocking)]
    fn get_proof(
        &self,
        account: Address,
        keys: Vec<B256>,
        batch_number: u64,
    ) -> RpcResult<Option<BatchStorageProof>>;

    /// Returns the number of the latest block that contained a real
    /// `SetSLChainId` system transaction — i.e. the last block at which this
    /// chain's settlement layer changed. The `u64::MAX` sentinel emitted by
    /// the v31 protocol upgrade is excluded.
    ///
    /// Returns `None` if no settlement-layer change has ever happened on this
    /// chain (i.e. since genesis).
    ///
    /// Operators driving a gateway migration can use this to find the block at
    /// which the batcher quiesced, wait for that block's batch to reach L1
    /// finality, and then restart the server with the new settlement-layer
    /// configuration.
    #[method(name = "lastSettlementChangeBlock")]
    async fn last_settlement_change_block(&self) -> RpcResult<Option<u64>>;
}
