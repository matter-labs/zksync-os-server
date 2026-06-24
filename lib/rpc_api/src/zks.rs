use crate::types::{
    BatchStorageProof, BlockMetadata, ImtInclusionProof, L2ToL1LogProof, LogProofTarget,
};
use alloy::primitives::{Address, B256, TxHash, U256};
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

    /// Returns the IMT membership proof for the atomic-interop commit leaf holding `commit_value`,
    /// against this chain's commitment tree (`L2InteropCommitmentTree`) as of `block_number`.
    ///
    /// `block_number` must be the L2 block whose IMT root the caller's message proof authenticates
    /// (typically the atomic-send block). Returns `None` if no leaf holds `commit_value` at that block.
    #[method(name = "getImtInclusionProof")]
    async fn get_imt_inclusion_proof(
        &self,
        commit_value: U256,
        block_number: u64,
    ) -> RpcResult<Option<ImtInclusionProof>>;

    /// Returns the index of the low-nullifier leaf for `value` (the predecessor used when inserting
    /// `value`) in this chain's commitment tree as of `block_number`, or `None` if none exists.
    ///
    /// Used at atomic-send time to supply the IMT insert's low-nullifier index without an off-chain
    /// tree reconstruction in the client.
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
