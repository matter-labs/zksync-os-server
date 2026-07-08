use alloy::primitives::ruint::aliases::B160;
use alloy::primitives::{Address, B256, address};
use zk_ee::common_structs::derive_flat_storage_key;
use zksync_os_interface::traits::ReadStorage;

/// Reads the atomic-interop commitment tree (IMT) root from the
/// `L2InteropCommitmentTree` (0x10012) contract state.
///
/// The contract caches its current root in `_currentRoot` at **fixed slot 0** — a deliberate,
/// consensus-critical storage ABI (see `L2InteropCommitmentTree.sol`): the underlying
/// dynamic-height engine has no fixed root slot, so the cache is what both the ZKsync OS
/// bootloader (batch begin/end snapshots committed into the chain batch root) and this reader
/// consume. An uninitialized or absent tree reads as `B256::ZERO`, matching the bootloader's
/// reading on chains without the atomic stack.
pub fn read_commitment_tree_root(mut state: impl ReadStorage) -> B256 {
    const L2_INTEROP_COMMITMENT_TREE_ADDRESS: Address =
        address!("0x0000000000000000000000000000000000010012");
    const CURRENT_ROOT_SLOT: B256 = B256::ZERO;

    let flat_key = derive_flat_storage_key(
        &B160::from_be_bytes(L2_INTEROP_COMMITMENT_TREE_ADDRESS.into_array()),
        &CURRENT_ROOT_SLOT.0.into(),
    );
    state
        .read(flat_key.as_u8_array().into())
        .unwrap_or_default()
}
