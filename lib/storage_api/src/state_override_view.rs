use std::collections::HashMap;

use crate::ViewState;
use alloy::primitives::B256;
use alloy::primitives::ruint::aliases::B160;
use alloy::rpc::types::state::StateOverride;
use zk_ee::common_structs::derive_flat_storage_key;
use zk_os_api::helpers::{set_properties_balance, set_properties_code, set_properties_nonce};
use zk_os_basic_system::system_implementation::flat_storage_model::{
    ACCOUNT_PROPERTIES_STORAGE_ADDRESS, AccountProperties, address_into_special_storage_key,
};
use zksync_os_interface::traits::{PreimageSource, ReadStorage};

/// A `ViewState` wrapper that overrides specific storage slots.
/// All other reads/preimage lookups delegate to the inner state.
#[derive(Debug, Clone)]
pub struct OverriddenStateView<V: ViewState> {
    inner: V,
    // unified storage slot overrides (flat keys) including account property slots
    overrides: HashMap<B256, B256>,
    // preimage overrides: hash -> preimage bytes (for account props and bytecode)
    preimage_overrides: HashMap<B256, Vec<u8>>,
}

impl<V: ViewState> OverriddenStateView<V> {
    pub fn new(inner: V, state_overrides: StateOverride) -> Self {
        let (overrides, preimage_overrides) = build_state_override_maps(&inner, state_overrides);

        Self {
            inner,
            overrides,
            preimage_overrides,
        }
    }
}

impl<V: ViewState> ReadStorage for OverriddenStateView<V> {
    fn read(&mut self, key: B256) -> Option<B256> {
        if let Some(val) = self.overrides.get(&key) {
            return Some(*val);
        }

        self.inner.read(key)
    }
}

impl<V: ViewState> PreimageSource for OverriddenStateView<V> {
    fn get_preimage(&mut self, hash: B256) -> Option<Vec<u8>> {
        if let Some(bytes) = self.preimage_overrides.get(&hash) {
            return Some(bytes.clone());
        }

        self.inner.get_preimage(hash)
    }
}

/// Converts RPC `StateOverride` into unified slot overrides and preimage overrides.
fn build_state_override_maps<V: ViewState>(
    inner: &V,
    state_overrides: StateOverride,
) -> (HashMap<B256, B256>, HashMap<B256, Vec<u8>>) {
    let mut overrides: HashMap<B256, B256> = HashMap::new();
    let mut preimage_overrides: HashMap<B256, Vec<u8>> = HashMap::new();

    // `StateOverride` is a map-like structure of Address => AccountOverride
    for (address, account) in state_overrides {
        // Merge `state` and `state_diff` if present. Latter should take precedence on overlap.
        if let Some(state) = account.state {
            for (slot, value) in state {
                let flat_key = derive_flat_storage_key(
                    &B160::from_be_bytes(address.into_array()),
                    &(slot.0.into()),
                );
                overrides.insert(B256::from(flat_key.as_u8_array()), value);
            }
        }
        if let Some(state_diff) = account.state_diff {
            for (slot, value_override) in state_diff {
                let flat_key = derive_flat_storage_key(
                    &B160::from_be_bytes(address.into_array()),
                    &(slot.0.into()),
                );
                overrides.insert(B256::from(flat_key.as_u8_array()), value_override);
            }
        }

        if account.balance.is_some() || account.nonce.is_some() || account.code.is_some() {
            // start from current account props if present
            let mut base: AccountProperties =
                inner.clone().get_account(address).unwrap_or_default();

            if let Some(nonce) = account.nonce {
                set_properties_nonce(&mut base, nonce);
            }
            if let Some(balance) = account.balance {
                set_properties_balance(&mut base, balance);
            }
            if let Some(code) = account.code {
                let bytecode_preimage = set_properties_code(&mut base, &code);
                let bytecode_hash_b256: B256 = base.bytecode_hash.as_u8_array().into();
                preimage_overrides.insert(bytecode_hash_b256, bytecode_preimage);
            }

            // Compute and store account properties preimage and its hash
            let acc_hash = base.compute_hash();
            let acc_hash_b256: B256 = acc_hash.as_u8_array().into();
            preimage_overrides.insert(acc_hash_b256, base.encoding().to_vec());

            // Compute flat storage key for account properties of this address and override it
            let key = derive_flat_storage_key(
                &ACCOUNT_PROPERTIES_STORAGE_ADDRESS,
                &address_into_special_storage_key(&B160::from_be_bytes(address.into_array())),
            );
            overrides.insert(B256::from(key.as_u8_array()), acc_hash_b256);
        }
    }

    (overrides, preimage_overrides)
}
