use alloy::primitives::ruint::aliases::B160;
use alloy::primitives::{Address, B256, U256, keccak256};
use zk_ee::common_structs::derive_flat_storage_key;
use zksync_os_interface::traits::ReadStorage;

/// Reads the root of a `DynamicIncrementalMerkle`-style tree from contract storage:
/// `nodes[height][0]`, with the height word at `height_key` and the nodes array base at
/// `nodes_key`. A consensus-critical storage ABI shared with the bootloader; an uninitialized
/// tree reads as `B256::ZERO`.
pub(crate) fn read_dynamic_tree_root(
    mut state: impl ReadStorage,
    contract: Address,
    height_key: B256,
    nodes_key: B256,
) -> B256 {
    let contract = B160::from_be_bytes(contract.into_array());
    let tree_height = {
        let flat_key = derive_flat_storage_key(&contract, &height_key.0.into());
        state
            .read(flat_key.as_u8_array().into())
            .unwrap_or_default()
    };

    // `nodes[height][0]`
    let root_key = n_dim_array_key_in_layout(nodes_key, &[tree_height, B256::ZERO]);
    let flat_key = derive_flat_storage_key(&contract, &root_key.0.into());
    state
        .read(flat_key.as_u8_array().into())
        .unwrap_or_default()
}

pub(crate) fn n_dim_array_key_in_layout(array_key: B256, indices: &[B256]) -> B256 {
    let mut key = array_key;

    for index in indices {
        let hashed = U256::from_be_bytes(keccak256(key.0).0);
        let index_u256 = U256::from_be_bytes(index.0);
        key = B256::from(hashed.overflowing_add(index_u256).0);
    }

    key
}
