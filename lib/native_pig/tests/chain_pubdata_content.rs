//! The chain's pubdata content is pinned once per process, so these assertions live in their own
//! test binary rather than next to the crate's unit tests.

use zksync_os_native_pig::{
    PubdataContent, chain_pubdata_content, set_chain_pubdata_content, v32_chain_config,
    v32_chain_config_hash,
};

const CHAIN_ID: u64 = 270;

#[test]
fn pinning_logs_only_changes_the_chain_config_and_its_hash() {
    // Unset, the process behaves as every rollup and every pre-v32 chain does.
    assert_eq!(chain_pubdata_content(), PubdataContent::FullPubdata);
    let full_pubdata_hash = v32_chain_config_hash(CHAIN_ID).unwrap();

    set_chain_pubdata_content(PubdataContent::LogsOnly).unwrap();

    assert_eq!(chain_pubdata_content(), PubdataContent::LogsOnly);
    assert_eq!(
        v32_chain_config(CHAIN_ID).unwrap().pubdata_content(),
        PubdataContent::LogsOnly
    );
    // The content is part of the chain config hash, and therefore of the batch public input.
    assert_ne!(v32_chain_config_hash(CHAIN_ID).unwrap(), full_pubdata_hash);

    // Re-pinning the same value is a no-op; a conflicting one is refused, since a running node must
    // never execute under two different chain configs.
    set_chain_pubdata_content(PubdataContent::LogsOnly).unwrap();
    assert!(set_chain_pubdata_content(PubdataContent::FullPubdata).is_err());
}
