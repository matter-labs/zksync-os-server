use alloy::primitives::{Address, address};

pub(crate) const ASSET_TRACKER_ADDRESS: Address =
    address!("0x000000000000000000000000000000000001000f");
pub(crate) const ASSET_TRACKER_ROOT_SELECTOR: [u8; 4] = [0x03, 0x11, 0x7c, 0x8c];
/// The L2 base token system contract, which is the caller of the bootloader-injected
/// asset-tracker root frames.
pub(crate) const L2_BASE_TOKEN_ADDRESS: Address =
    address!("0x000000000000000000000000000000000000800a");

pub(crate) fn is_asset_tracker_root_call(from: Address, to: Option<Address>, input: &[u8]) -> bool {
    from == L2_BASE_TOKEN_ADDRESS
        && to == Some(ASSET_TRACKER_ADDRESS)
        && input.starts_with(&ASSET_TRACKER_ROOT_SELECTOR)
}

pub(crate) fn without_ignored_roots<T>(
    roots: Vec<T>,
    mut is_ignored_root: impl FnMut(&T) -> bool,
) -> Vec<T> {
    let (ignored, kept): (Vec<_>, Vec<_>) =
        roots.into_iter().partition(|root| is_ignored_root(root));

    if kept.is_empty() { ignored } else { kept }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_tracker_root_call_must_come_from_l2_base_token() {
        assert!(is_asset_tracker_root_call(
            L2_BASE_TOKEN_ADDRESS,
            Some(ASSET_TRACKER_ADDRESS),
            &ASSET_TRACKER_ROOT_SELECTOR,
        ));

        assert!(!is_asset_tracker_root_call(
            Address::from([0x11; 20]),
            Some(ASSET_TRACKER_ADDRESS),
            &ASSET_TRACKER_ROOT_SELECTOR,
        ));
    }
}
