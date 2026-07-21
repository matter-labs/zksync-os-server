//! The genesis state for real-STF simulation: the production genesis, plus funded
//! test accounts.
//!
//! Derived through the same `build_genesis` the node uses, from the same genesis input
//! file (`local-chains/v31.0/genesis.json`), so simulated chains start from a faithful
//! protocol-v31 state. Construction costs a few hundred milliseconds (bytecode artifact
//! computation for the system contracts), so the result is built once per process and
//! shared — it is immutable, which also keeps runs reproducible.

use alloy::primitives::ruint::aliases::B160;
use alloy::primitives::{Address, B256, U256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use zk_ee::common_structs::derive_flat_storage_key;
use zk_os_api::helpers::set_properties_balance;
use zk_os_basic_system::system_implementation::flat_storage_model::{
    ACCOUNT_PROPERTIES_STORAGE_ADDRESS, AccountProperties, address_into_special_storage_key,
};
use zksync_os_genesis::{FileGenesisInputSource, build_genesis};
use zksync_os_storage_api::BlockContext;

/// The immutable starting state every simulated validator shares.
pub struct SharedGenesis {
    pub storage: HashMap<B256, B256>,
    pub preimages: HashMap<B256, Vec<u8>>,
    /// The genesis block's context — the source of chain id, fee constants, and the
    /// initial block-hash ring for building block 1's context.
    pub context: BlockContext,
    /// Hash of the genesis block header (seed of the block-hash ring).
    pub header_hash: B256,
}

pub fn account_properties_flat_key(address: Address) -> B256 {
    let key = derive_flat_storage_key(
        &ACCOUNT_PROPERTIES_STORAGE_ADDRESS,
        &address_into_special_storage_key(&B160::from_be_bytes(address.into_array())),
    );
    B256::from(key.as_u8_array())
}

fn genesis_json_path() -> PathBuf {
    // <repo>/lib/consensus/sim -> <repo>/local-chains/v31.0/genesis.json
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../local-chains/v31.0/genesis.json")
}

/// Builds (once per process) the production genesis state extended with funded accounts.
pub fn shared_genesis(funded: &[(Address, U256)]) -> Arc<SharedGenesis> {
    static GENESIS: OnceLock<Arc<SharedGenesis>> = OnceLock::new();
    let funded = funded.to_vec();
    GENESIS
        .get_or_init(move || {
            let source = FileGenesisInputSource::new(genesis_json_path());
            // `build_genesis` is async only because genesis inputs can come from remote
            // sources; the file source completes immediately, so a trivial executor is
            // enough — no runtime needed.
            let state = futures::executor::block_on(build_genesis(
                &source,
                270, // chain id of the local-chains v31.0 genesis
                &"0.31.0".parse().expect("valid protocol version"),
            ))
            .expect("failed to build genesis state");

            let mut storage: HashMap<B256, B256> = state.storage_logs.into_iter().collect();
            let mut preimages: HashMap<B256, Vec<u8>> = state.preimages.into_iter().collect();

            // Fund the test accounts. The production genesis has no funded EOAs (funds
            // normally arrive via L1 deposits, which the simulation does not model).
            for (address, balance) in &funded {
                let mut properties = AccountProperties::default();
                set_properties_balance(&mut properties, *balance);
                let properties_hash = properties.compute_hash();
                storage.insert(
                    account_properties_flat_key(*address),
                    properties_hash.as_u8_array().into(),
                );
                preimages.insert(
                    properties_hash.as_u8_array().into(),
                    properties.encoding().to_vec(),
                );
            }

            Arc::new(SharedGenesis {
                storage,
                preimages,
                context: state.context,
                header_hash: state.header.hash(),
            })
        })
        .clone()
}
