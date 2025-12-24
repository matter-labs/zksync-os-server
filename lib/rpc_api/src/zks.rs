use crate::types::{BlockMetadata, L2ToL1LogProof};
use alloy::primitives::{Address, TxHash};
use alloy::rpc::types::Index;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use zksync_os_genesis::GenesisInput;

#[cfg_attr(not(feature = "server"), rpc(client, namespace = "zks"))]
#[cfg_attr(feature = "server", rpc(server, client, namespace = "zks"))]
pub trait ZksApi {
    #[method(name = "getBridgehubContract")]
    async fn get_bridgehub_contract(&self) -> RpcResult<Address>;

    #[method(name = "getBytecodeSupplierContract")]
    async fn get_bytecode_supplier_contract(&self) -> RpcResult<Address>;

    #[method(name = "getL2ToL1LogProof")]
    async fn get_l2_to_l1_log_proof(
        &self,
        tx_hash: TxHash,
        index: Index,
    ) -> RpcResult<Option<L2ToL1LogProof>>;

    #[method(name = "getGenesis")]
    async fn get_genesis(&self) -> RpcResult<GenesisInput>;

    #[method(name = "getBlockMetadataByNumber")]
    async fn get_block_metadata_by_number(
        &self,
        block_number: u64,
    ) -> RpcResult<Option<BlockMetadata>>;
}
