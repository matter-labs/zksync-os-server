use alloy::primitives::ruint::aliases::B160;
use alloy::primitives::{Address, B256, U256, address, keccak256};
use zk_ee::common_structs::derive_flat_storage_key;
use zksync_os_interface::traits::ReadStorage;

pub fn read_multichain_root(mut state: impl ReadStorage) -> B256 {
    const L2_MESSAGE_ROOT_ADDRESS: Address = address!("0x0000000000000000000000000000000000010005");
    const AGG_TREE_HEIGHT_KEY: B256 = B256::with_last_byte(0x04);
    const AGG_TREE_NODES_KEY: B256 = B256::with_last_byte(0x06);

    let agg_tree_height = {
        let flat_key = derive_flat_storage_key(
            &B160::from_be_bytes(L2_MESSAGE_ROOT_ADDRESS.into_array()),
            &AGG_TREE_HEIGHT_KEY.0.into(),
        );
        state
            .read(flat_key.as_u8_array().into())
            .unwrap_or_default()
    };

    // `nodes[height][0]`
    let agg_tree_root_hash_key =
        n_dim_array_key_in_layout(AGG_TREE_NODES_KEY, &[agg_tree_height, B256::ZERO]);
    let flat_key = derive_flat_storage_key(
        &B160::from_be_bytes(L2_MESSAGE_ROOT_ADDRESS.into_array()),
        &agg_tree_root_hash_key.0.into(),
    );
    state
        .read(flat_key.as_u8_array().into())
        .unwrap_or_default()
}

fn n_dim_array_key_in_layout(array_key: B256, indices: &[B256]) -> B256 {
    let mut key = array_key;

    for index in indices {
        let hashed = U256::from_be_bytes(keccak256(key.0).0);
        let index_u256 = U256::from_be_bytes(index.0);
        key = B256::from(hashed.overflowing_add(index_u256).0);
    }

    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::b256;

    #[test]
    fn test_calculate_multichain_root_slot_tree_height_4() {
        const AGG_TREE_NODES_KEY: B256 = B256::with_last_byte(0x06);

        let agg_tree_height = B256::with_last_byte(0x4);
        let agg_tree_root_hash_key =
            n_dim_array_key_in_layout(AGG_TREE_NODES_KEY, &[agg_tree_height, B256::ZERO]);

        assert_eq!(
            agg_tree_root_hash_key,
            b256!("0x35817d789b7a6dbe8b95b0f21e189fb26d3d329de699cac7a267a9568298e0a5")
        );
    }
}
