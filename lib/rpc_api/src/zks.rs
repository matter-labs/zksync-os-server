use crate::types::{
    BatchStorageProof, BlockMetadata, ImtInclusionProof, L2ToL1LogProof, LogProofTarget,
};
use alloy::primitives::{Address, B256, TxHash, U256};
use alloy::rpc::types::Index;
// In client-only mode the `rpc` macro replaces `RpcResult` return types with
// `Result<_, ClientError>`, leaving this import unused.
#[cfg(feature = "server")]
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
    /// If omitted, [`LogProofTarget::L1BatchRoot`] is used. A `MessageRoot` proof is available only
    /// after the source batch has executed on L1 and only for protocol versions that support L1
    /// interop.
    #[method(name = "getL2ToL1LogProof")]
    async fn get_l2_to_l1_log_proof(
        &self,
        tx_hash: TxHash,
        index: Index,
        proof_target: Option<LogProofTarget>,
    ) -> RpcResult<Option<L2ToL1LogProof>>;

    /// Returns the IMT membership proof for the atomic-interop leaf holding `commit_value`.
    ///
    /// The tree is read at `block_number`, normally the atomic-send block. The caller then uses a
    /// message proof to authenticate the IMT root published by that block. Returns `None` if the
    /// value was not present then.
    #[method(name = "getImtInclusionProof")]
    async fn get_imt_inclusion_proof(
        &self,
        commit_value: U256,
        block_number: u64,
    ) -> RpcResult<Option<ImtInclusionProof>>;

    /// Returns the low-nullifier leaf index needed to insert `value` against the tree state at
    /// `block_number`.
    ///
    /// This is the predecessor that brackets the new value in the IMT's sorted linked list. For
    /// leaves `5 -> 9`, inserting `7` returns the index of leaf `5`. Clients use this before the
    /// atomic-send transaction instead of reconstructing the tree themselves.
    #[method(name = "getImtLowNullifierIndex")]
    async fn get_imt_low_nullifier_index(
        &self,
        value: U256,
        block_number: u64,
    ) -> RpcResult<Option<u64>>;

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
}
