//! Reads the atomic-interop commitment tree from historical local L2 state.
//!
//! The authoritative leaf preimages live in the fixed-address `L2InteropCommitmentTree` system
//! contract; the server does not maintain a separate historical copy of the tree. This reader
//! reuses the node's local `eth_call` path to execute `leafCount()` and `leafAt(i)` against the
//! requested block's state. The contract exposes leaves but no Merkle paths, so [`crate::imt`]
//! reconstructs the internal nodes and paths from those leaves.

use crate::eth_call_handler::{EthCallError, EthCallHandler};
use crate::imt::{ImtLeaf, IndexedMerkleTree};
use crate::rpc_storage::ReadRpcStorage;
use alloy::eips::BlockId;
use alloy::primitives::{B256, Bytes, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use zksync_os_contract_interface::IL2InteropCommitmentTree;
use zksync_os_types::L2_INTEROP_COMMITMENT_TREE_ADDRESS;

/// Failure while reconstructing the atomic-interop commitment tree.
#[derive(Debug, thiserror::Error)]
pub enum InteropCommitmentTreeError {
    #[error("commitment-tree `{method}` call failed")]
    Call {
        method: &'static str,
        #[source]
        source: EthCallError,
    },
    #[error("failed to decode commitment-tree `{method}` response")]
    Decode {
        method: &'static str,
        #[source]
        source: alloy::sol_types::Error,
    },
    #[error("commitment tree has no sentinel leaf")]
    MissingSentinel,
    #[error(
        "IMT inclusion proof failed self-verification for commit value {commit_value} at leaf \
         {leaf_index}: recomputed root {recomputed_root} != tree root {tree_root}"
    )]
    ProofMismatch {
        commit_value: U256,
        leaf_index: u64,
        recomputed_root: B256,
        tree_root: B256,
    },
}

/// Loads index-ordered commitment-tree leaves through this node's local `eth_call` implementation.
pub(crate) struct InteropCommitmentTreeReader<RpcStorage> {
    eth_call_handler: EthCallHandler<RpcStorage>,
}

impl<RpcStorage> InteropCommitmentTreeReader<RpcStorage> {
    pub(crate) fn new(eth_call_handler: EthCallHandler<RpcStorage>) -> Self {
        Self { eth_call_handler }
    }
}

impl<RpcStorage: ReadRpcStorage> InteropCommitmentTreeReader<RpcStorage> {
    fn call(
        &self,
        method: &'static str,
        calldata: Bytes,
        block: BlockId,
    ) -> Result<Bytes, InteropCommitmentTreeError> {
        let request = TransactionRequest::default()
            .to(L2_INTEROP_COMMITMENT_TREE_ADDRESS)
            .input(calldata.into());
        self.eth_call_handler
            .call_impl(request, Some(block), None, None)
            .map_err(|source| InteropCommitmentTreeError::Call { method, source })
    }

    /// Rebuilds the tree because the contract exposes historical leaves, but not Merkle paths.
    pub(crate) fn read(
        &self,
        block: BlockId,
    ) -> Result<IndexedMerkleTree, InteropCommitmentTreeError> {
        let count_bytes = self.call(
            "leafCount",
            IL2InteropCommitmentTree::leafCountCall {}
                .abi_encode()
                .into(),
            block,
        )?;
        let leaf_count = IL2InteropCommitmentTree::leafCountCall::abi_decode_returns(&count_bytes)
            .map_err(|source| InteropCommitmentTreeError::Decode {
                method: "leafCount",
                source,
            })?
            .to::<u64>();
        if leaf_count == 0 {
            return Err(InteropCommitmentTreeError::MissingSentinel);
        }

        let mut leaves = Vec::with_capacity(leaf_count as usize);
        for index in 0..leaf_count {
            let leaf_bytes = self.call(
                "leafAt",
                IL2InteropCommitmentTree::leafAtCall {
                    index: U256::from(index),
                }
                .abi_encode()
                .into(),
                block,
            )?;
            let leaf = IL2InteropCommitmentTree::leafAtCall::abi_decode_returns(&leaf_bytes)
                .map_err(|source| InteropCommitmentTreeError::Decode {
                    method: "leafAt",
                    source,
                })?;
            leaves.push(ImtLeaf {
                value: leaf.value,
                next_index: leaf.nextIndex,
                next_value: leaf.nextValue,
            });
        }

        Ok(IndexedMerkleTree::new(leaves))
    }
}
