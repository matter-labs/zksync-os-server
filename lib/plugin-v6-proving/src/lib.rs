//! Proving plugin for zksync-os V6.
//!
//! Wraps `zk_os_forward_system`'s proof generation functions behind the
//! [`ProvingPlugin`] trait and a standalone [`generate_proof_input`] function.
//! This crate is the only place outside `lib/merkle_tree` that depends on
//! `zk_os_forward_system` for proving — callers use this crate instead.

use std::collections::VecDeque;
use std::path::PathBuf;

use zk_ee::common_structs::DACommitmentScheme;
use zk_os_forward_system::run::StorageCommitment;
use zksync_os_interface::traits::{EncodedTx, PreimageSource, TxListSource};
use zksync_os_interface::types::BlockContext;
use zksync_os_merkle_tree::{MerkleTreeVersion, RocksDBWrapper, fixed_bytes_to_bytes32};

pub use alloy::primitives::B256;
use zksync_os_plugin_api::ProvingPlugin;

pub struct PluginV6Proving;

impl ProvingPlugin for PluginV6Proving {
    fn generate_batch_proof_input(
        &self,
        block_proof_inputs: Vec<&[u32]>,
        da_commitment_scheme: u8,
        pubdata: Vec<&[u8]>,
    ) -> Result<Vec<u32>, anyhow::Error> {
        let da_commitment_scheme: DACommitmentScheme = da_commitment_scheme
            .try_into()
            .map_err(|_| anyhow::anyhow!("Failed to convert DA commitment scheme"))?;
        Ok(zk_os_forward_system::run::generate_batch_proof_input(
            block_proof_inputs,
            da_commitment_scheme,
            pubdata,
        ))
    }
}

/// Generate per-block proof input for proving version V6.
///
/// This is a standalone function (not on the trait) because it requires concrete
/// types from `merkle_tree` and `zksync_os_interface` that cannot be abstracted
/// without pulling `forward_system` types into the plugin API.
#[allow(clippy::too_many_arguments)]
pub fn generate_proof_input<PS: PreimageSource>(
    app_bin_path: PathBuf,
    block_context: BlockContext,
    previous_block_timestamp: u64,
    root_hash: B256,
    leaf_count: u64,
    da_commitment_scheme: u8,
    tree_view: MerkleTreeVersion<RocksDBWrapper>,
    preimage_source: PS,
    transactions: VecDeque<EncodedTx>,
) -> Result<Vec<u32>, anyhow::Error> {
    use zk_ee::common_structs::ProofData;
    use zk_ee::system::metadata::zk_metadata::BlockMetadataFromOracle;
    use zk_os_forward_system::run::convert::FromInterface;

    let da_commitment_scheme: DACommitmentScheme = da_commitment_scheme
        .try_into()
        .map_err(|_| anyhow::anyhow!("Failed to convert DA commitment scheme"))?;

    let initial_storage_commitment = StorageCommitment {
        root: fixed_bytes_to_bytes32(root_hash).as_u8_array().into(),
        next_free_slot: leaf_count,
    };

    let list_source = TxListSource { transactions };

    zk_os_forward_system::run::generate_proof_input(
        app_bin_path,
        BlockMetadataFromOracle::from_interface(block_context),
        ProofData {
            state_root_view: initial_storage_commitment,
            last_block_timestamp: previous_block_timestamp,
        },
        da_commitment_scheme,
        tree_view,
        preimage_source,
        list_source,
    )
    .map_err(|e| anyhow::anyhow!(e))
}
