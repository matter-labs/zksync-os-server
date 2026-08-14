use crate::read_dynamic_tree_root::read_dynamic_tree_root;
use alloy::primitives::{Address, B256, address};
use zksync_os_interface::traits::ReadStorage;

/// Reads the aggregated multichain root from the `L2MessageRoot` (0x10005) contract state:
/// `nodes[height][0]` with the height at slot 4 and the nodes base at slot 6.
pub fn read_multichain_root(state: impl ReadStorage) -> B256 {
    const L2_MESSAGE_ROOT_ADDRESS: Address = address!("0x0000000000000000000000000000000000010005");
    const AGG_TREE_HEIGHT_KEY: B256 = B256::with_last_byte(0x04);
    const AGG_TREE_NODES_KEY: B256 = B256::with_last_byte(0x06);

    read_dynamic_tree_root(
        state,
        L2_MESSAGE_ROOT_ADDRESS,
        AGG_TREE_HEIGHT_KEY,
        AGG_TREE_NODES_KEY,
    )
}

#[cfg(test)]
mod tests {
    use crate::read_dynamic_tree_root::n_dim_array_key_in_layout;
    use alloy::primitives::{B256, b256};

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
