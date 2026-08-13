use alloy::primitives::ruint::aliases::B160;
use alloy::primitives::{Address, B256, U256, address, keccak256};
use zk_ee::common_structs::derive_flat_storage_key;
use zksync_os_interface::traits::ReadStorage;

/// Reads the atomic-interop commitment tree (IMT) root from the
/// `L2InteropCommitmentTree` (0x10012) contract state.
///
/// The root lives at `_imt.tree._nodes[_height][0]` (`_height` at slot 0, `_nodes` base at
/// slot 2) — a consensus-critical storage ABI shared with the bootloader. An uninitialized
/// tree reads as `B256::ZERO`.
pub fn read_commitment_tree_root(mut state: impl ReadStorage) -> B256 {
    const L2_INTEROP_COMMITMENT_TREE_ADDRESS: Address =
        address!("0x0000000000000000000000000000000000010012");
    const IMT_TREE_HEIGHT_KEY: B256 = B256::ZERO;
    const IMT_TREE_NODES_KEY: B256 = B256::with_last_byte(0x02);

    let tree_height = {
        let flat_key = derive_flat_storage_key(
            &B160::from_be_bytes(L2_INTEROP_COMMITMENT_TREE_ADDRESS.into_array()),
            &IMT_TREE_HEIGHT_KEY.0.into(),
        );
        state
            .read(flat_key.as_u8_array().into())
            .unwrap_or_default()
    };

    // `_nodes[height][0]`
    let root_key = n_dim_array_key_in_layout(IMT_TREE_NODES_KEY, &[tree_height, B256::ZERO]);
    let flat_key = derive_flat_storage_key(
        &B160::from_be_bytes(L2_INTEROP_COMMITMENT_TREE_ADDRESS.into_array()),
        &root_key.0.into(),
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

    // Expected values: keccak256(keccak256(uint256(2)) + height), cross-checked against the
    // bootloader's `calculate_imt_root_slot` and the solidity layout lock test
    // (`L2InteropCommitmentTreeStorage.t.sol`).
    #[test]
    fn test_imt_root_slot_tree_height_0() {
        const IMT_TREE_NODES_KEY: B256 = B256::with_last_byte(0x02);

        let root_key = n_dim_array_key_in_layout(IMT_TREE_NODES_KEY, &[B256::ZERO, B256::ZERO]);

        assert_eq!(
            root_key,
            b256!("0x1ab0c6948a275349ae45a06aad66a8bd65ac18074615d53676c09b67809099e0")
        );
    }

    #[test]
    fn test_imt_root_slot_tree_height_4() {
        const IMT_TREE_NODES_KEY: B256 = B256::with_last_byte(0x02);

        let root_key = n_dim_array_key_in_layout(
            IMT_TREE_NODES_KEY,
            &[B256::with_last_byte(0x04), B256::ZERO],
        );

        assert_eq!(
            root_key,
            b256!("0xcc034019b449ad16908580172ec972745a229ec6575a8d785eaa22043f92c453")
        );
    }
}
