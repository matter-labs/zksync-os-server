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

/// Trait for providing storage and preimage overrides.
/// Allows different implementations: owned HashMaps for RPC calls, or shared data for sequencer.
/// Requires 'static because it's used in types that implement ReadStorage/PreimageSource.
pub trait OverrideProvider: 'static {
    /// Look up a storage override by key.
    fn get_storage_override(&self, key: &B256) -> Option<B256>;

    /// Look up a preimage override by hash.
    fn get_preimage_override(&self, hash: &B256) -> Option<Vec<u8>>;
}

/// Owned HashMap-based override provider.
/// Used for RPC calls with StateOverride, where we own the override data.
#[derive(Debug, Clone, Default)]
pub struct OwnedOverrides {
    storage: HashMap<B256, B256>,
    preimages: HashMap<B256, Vec<u8>>,
}

impl OwnedOverrides {
    pub fn new(storage: HashMap<B256, B256>, preimages: HashMap<B256, Vec<u8>>) -> Self {
        Self { storage, preimages }
    }
}

impl OverrideProvider for OwnedOverrides {
    fn get_storage_override(&self, key: &B256) -> Option<B256> {
        self.storage.get(key).copied()
    }

    fn get_preimage_override(&self, hash: &B256) -> Option<Vec<u8>> {
        self.preimages.get(hash).cloned()
    }
}

/// A `ViewState` wrapper that overrides specific storage slots.
/// All other reads/preimage lookups delegate to the inner state.
/// Generic over both the inner state V and the override provider O.
#[derive(Debug, Clone)]
pub struct OverriddenStateView<V: ViewState, O: OverrideProvider> {
    inner: V,
    overrides: O,
}

impl<V: ViewState, O: OverrideProvider> OverriddenStateView<V, O> {
    pub fn new(inner: V, overrides: O) -> Self {
        Self { inner, overrides }
    }
}

// Convenience constructors for common cases
impl<V: ViewState> OverriddenStateView<V, OwnedOverrides> {
    /// Create from RPC StateOverride.
    pub fn with_state_overrides(inner: V, state_overrides: StateOverride) -> Self {
        let overrides = build_state_override_maps(&inner, state_overrides);
        Self::new(inner, overrides)
    }

    /// Create with only preimage overrides.
    pub fn with_preimages(inner: V, preimage_overrides: &[(B256, Vec<u8>)]) -> Self {
        let preimages = preimage_overrides
            .iter()
            .cloned()
            .collect::<HashMap<B256, Vec<u8>>>();
        Self::new(inner, OwnedOverrides::new(HashMap::new(), preimages))
    }
}

impl<V: ViewState, O: OverrideProvider> ReadStorage for OverriddenStateView<V, O> {
    fn read(&mut self, key: B256) -> Option<B256> {
        if let Some(val) = self.overrides.get_storage_override(&key) {
            return Some(val);
        }

        self.inner.read(key)
    }
}

impl<V: ViewState, O: OverrideProvider> PreimageSource for OverriddenStateView<V, O> {
    fn get_preimage(&mut self, hash: B256) -> Option<Vec<u8>> {
        if let Some(bytes) = self.overrides.get_preimage_override(&hash) {
            return Some(bytes);
        }

        self.inner.get_preimage(hash)
    }
}

/// Converts RPC `StateOverride` into an `OwnedOverrides` provider.
fn build_state_override_maps<V: ViewState>(
    inner: &V,
    state_overrides: StateOverride,
) -> OwnedOverrides {
    let mut storage: HashMap<B256, B256> = HashMap::new();
    let mut preimages: HashMap<B256, Vec<u8>> = HashMap::new();

    // `StateOverride` is a map-like structure of Address => AccountOverride
    for (address, account) in state_overrides {
        // Merge `state` and `state_diff` if present. Latter should take precedence on overlap.
        if let Some(state) = account.state {
            for (slot, value) in state {
                let flat_key = derive_flat_storage_key(
                    &B160::from_be_bytes(address.into_array()),
                    &(slot.0.into()),
                );
                storage.insert(B256::from(flat_key.as_u8_array()), value);
            }
        }
        if let Some(state_diff) = account.state_diff {
            for (slot, value_override) in state_diff {
                let flat_key = derive_flat_storage_key(
                    &B160::from_be_bytes(address.into_array()),
                    &(slot.0.into()),
                );
                storage.insert(B256::from(flat_key.as_u8_array()), value_override);
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
                preimages.insert(bytecode_hash_b256, bytecode_preimage);
            }

            // Compute and store account properties preimage and its hash
            let acc_hash = base.compute_hash();
            let acc_hash_b256: B256 = acc_hash.as_u8_array().into();
            preimages.insert(acc_hash_b256, base.encoding().to_vec());

            // Compute flat storage key for account properties of this address and override it
            let key = derive_flat_storage_key(
                &ACCOUNT_PROPERTIES_STORAGE_ADDRESS,
                &address_into_special_storage_key(&B160::from_be_bytes(address.into_array())),
            );
            storage.insert(B256::from(key.as_u8_array()), acc_hash_b256);
        }
    }

    OwnedOverrides::new(storage, preimages)
}
