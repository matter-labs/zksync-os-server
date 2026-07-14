use crate::types::{BatchStorageProof, BlockMetadata, L2ToL1LogProof, LogProofTarget};
use alloy::primitives::{Address, B256, Bytes, TxHash};
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

    /// Returns the 124-byte `AccountProperties::encoding()` preimage for
    /// the given account at the end of the given L1 batch.
    ///
    /// Each `AccountProperties` struct lives in the state tree as
    /// `blake2s(AccountProperties::encoding())` at a slot keyed by the
    /// account address under `ACCOUNT_PROPERTIES_STORAGE_ADDRESS`. The
    /// Merkle proof for that slot — returned by [`Self::get_proof`] —
    /// covers the hash, but not the preimage itself. This method exposes
    /// the preimage bytes so that clients can reconstruct the full
    /// `AccountProperties` struct and check individual fields (balance,
    /// nonce, observable bytecode hash, versioning data, etc.) against
    /// a zks_getProof-verified tree value.
    ///
    /// The canonical user of this RPC is selective-disclosure tooling
    /// that needs to prove claims like
    /// `balance_of(l1_commitment, address, balance)` or
    /// `observable_bytecode_hash(l1_commitment, address, hash)` inside
    /// a zk circuit. Such tooling must hash the preimage bytes inside
    /// the circuit and compare against the Merkle-proof-verified slot
    /// value; partial reconstruction from `eth_getBalance` /
    /// `eth_getCode` / `eth_getTransactionCount` is not sufficient
    /// because `versioning_data`, `bytecode_hash`, and `artifacts_len`
    /// are internal fields not otherwise exposed over the JSON-RPC
    /// surface.
    ///
    /// Returns `None` when the account does not exist at the queried
    /// batch (i.e., the account has never been touched), matching the
    /// semantics of a non-existing-slot `zks_getProof` result.
    #[method(name = "getAccountPreimage")]
    async fn get_account_preimage(
        &self,
        account: Address,
        batch_number: u64,
    ) -> RpcResult<Option<Bytes>>;
}
